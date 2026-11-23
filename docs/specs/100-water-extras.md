# 100 — Water extras: underwater view, ring culling, sea-state table, water GPU time

Status: proposed with ADR-2342 (re-cut onto v0.2.13 from the 10-30 water-extras branch, whose
ADR-1893 and spec number 88 it replaces; 88 is foliage wind). Extends `docs/specs/water.md`
(ADR-1399, ADR-1620), `docs/specs/87-water-finish.md` and `docs/specs/water-ocean.md` (ADR-1969);
closes the items listed in `water.md` § 5 except outline-shaped lakes. Crates:
`regia_terrain::water_body` (`cull`, `sea_state`, `gpu`) and `regia_framework::sea_weather`. Agent
page: `book/agents/water.md` ("Water extras"). Sources: public papers only (§ 11); no other
engine's documents or code.

**Every extra is off by default** (§ 8): a project that sets nothing draws the frames it drew
before, bit for bit, and the FFT ocean and the trochoidal swell are unchanged.

## 1. Purpose

Four things were left after the swell and the water finish:

1. A camera under a plane's surface saw nothing of the water (the surface was discarded from
   below and the scene was not fogged).
2. The ring grid drew every triangle, also the ones behind the camera.
3. A sea needs a state: wave height and length that follow the wind, not hand-written waves.
4. `water.state` did not say what the water costs on the GPU.

## 2. Ring frustum culling

The ring grid (`water.md` § 3) is built as index runs, `RingChunk`: the inner square, then each
ring's four sides (`build_ring_grid_chunks`; the triangles are ordered by chunk). Each chunk has a
box of offsets from the grid's center. Per frame `cull::visible_ranges` moves each chunk's box to
the world (center in `f64`), widens it horizontally by the swell's largest shift `Σ H` and
vertically by `Σ A` around the level, **both doubled for shoaling** (`shore::SHOAL_MAX`, the
largest a field can raise a wave), and tests it against the six planes of the view-projection
matrix (`frustum_planes`, `box_outside`). A body that rides the frame's FFT ocean (ADR-1969) is
blended from its shoaled swell and the ocean by a weight in `0..1`, so its vertex stays inside the
larger of the two reaches: the box takes the maximum of the doubled swell reach and the ocean's
reach at the instant (`gpu::ocean_reach`): the per-band peaks of the CPU mirror's maps on the
query grid (`OceanFrame::{max_rise, max_shift}`, the GPU's own maps on the 128 grid), plus, on a
finer drawn grid, the strict bound `Σ (|a| + |b|)` of the modes only that grid has (the 128 grid's
modes are the finer grid's, and the extras all lie in the last band), padded by 1 % and 1 cm.
Bilinear sampling and the wave fade never exceed a band's peak. The strict bound of every mode
alone is so loose that no chunk was ever culled over an ocean (the first run of the frame test
showed it). With culling on, the mirror costs one CPU transform per tick (shared with the queries
of that tick). Chunks that survive are merged into index ranges and
drawn with one `draw_indexed` each. Culling is conservative (a box is dropped only when wholly
outside one plane), so the picture does not change: the lavapipe test renders one view with and
without it and compares bytes, over a swell and over an ocean (on Metal, whose fast-math shader
libraries move a few seam pixels when the draw list changes, the swell test allows at most 32
pixels off by at most 4 levels; ADR-3373). `WaterSettings::ring_culling`
(default off) and
`WaterRenderer::set_ring_culling` switch it; stats `ring_chunks`, `ring_chunks_culled`,
`ring_triangles_drawn`.

## 3. Sea-state table (`sea_state`)

`SEA_TABLE` has one row per Beaufort force 0-12: wind at 10 m, significant height, peak
wavelength, crest-foam strength and foam onset. Height and wavelength follow the fully developed
Pierson–Moskowitz sea (`Hs ≈ 0.0213 U²`, `λp ≈ 0.83 U²`); the foam columns are this engine's own
look values. `SeaState::row` interpolates linearly in the wind and holds above force 12;
the field `SeaState::fetch` (held to 0.1..1; non-finite is 1) scales the height by `fetch` and the
wavelength by `√fetch` in `SeaState::height_and_wavelength`, the wavelength floored at 0.25 m
(force 0's row) (own simplification of fetch-limited growth; ADR-5608). `SeaState::swell(count)` builds the waves with
`Swell::open_sea` (valid, deterministic, travelling with the wind) and sets the foam; a sea under
1 mm high is `None`.

`WeatherSea { fetch, waves }` on a plane `WaterBody` puts its swell under the table:
`apply_weather_sea` (Update, after `sync_terrain_water`) rewrites `body.swell` from `SeaWind` when
the quantized wind (`quantize_wind`, 0.5 m/s steps) changes the result; a steady wind writes
nothing. `SeaWind` is a resource the host may set; `regia_framework::sea_weather::SeaWeatherPlugin`
fills it from the scene's surface wind `regia_scene::weather::SceneWind` (spec 99 § 3.3): its
**mean** wind (`mean_m_per_s`, no gusts: a gust does not raise a developed sea), copied unscaled
because `SceneWind` is already the wind 10 m up. `SceneWindMode::Off`, or no `SceneWind`, leaves
`SeaWind` as the host set it. The weather writes `SceneWind` in `SceneWindMode::Weather`; a host
or an agent can hold it in `Manual`.

**On a body with an FFT ocean** (`WaterBody::ocean`) the component drives the ocean's wind
instead of the swell (`WeatherSea::ocean_for`): `wind_speed` rounded to 1 m/s
(`OCEAN_WIND_STEP`; a new wind rebuilds the spectrum, so the step is coarser) and clamped to the
ocean's `0.5..=40`, `wind_direction` rounded to 5° (`OCEAN_HEADING_STEP_DEG`, exact on the axes);
a calm or non-finite wind keeps the authored heading; seed, fetch, spread and the look stay the
author's, and the body's own swell (the Low tier's and the shore blend's) is not touched. Waves are a pure function of wind, fetch and count, so the same weather gives
the same sea; a transition steps the sea in 0.5 m/s wind steps (the phases are fixed per wave, so
a step moves the surface a little: a pop of the swell, not of the picture).

## 4. Underwater view

`WaterGpu::underwater_at` finds the first plane whose surface height at the eye (swell included,
`WaterBody::surface`) is above the eye. The frame uniform carries that body's slot and the surface
height. Two things draw:

- **Fog** (`fs_fog`, a full-screen triangle before the surfaces): Beer–Lambert along the ray from
  the eye to the scene or to the surface above (`min(scene distance, distance to the surface)`),
  `T = exp(−(κ + σ) d)`; in-scatter is the column's radiance `scatter · light · σ/(κ + σ)` with the
  light attenuated by the eye's depth (`volume_radiance`), times `1 − T`. A composite frame
  replaces the pixel with `scene · T + in-scatter`; a blend frame lays `in-scatter` with alpha
  `1 − mean(T)`.
- **The surface from below** (`underside`): inside the critical angle (`sin θt = 1.333 sin θi < 1`,
  smooth over the last degrees) a window shows the sky (or the scene copy on a composite frame)
  refracted by Snell's law and dimmed by the water between; beyond it the surface mirrors the
  water column (total internal reflection). The reflected share is Schlick's approximation at the
  incident angle.

Caustics and sun shafts are an opt-in pass (`WaterCaustics`, § 10, ADR-2432); off, nothing is drawn.
Ribbons are not fogged (no `underwater_at` for them);
a plane's rectangle ends the fog at its border. The eye test uses the authored body, so an ocean
plane answers its FFT surface (the CPU mirror), not the drawn stand-in swell.
`WaterSettings::underwater` (default off: as before, nothing is drawn from below) and
`WaterRenderer::set_underwater` switch it; stats `underwater`, `underwater_depth_m`.

## 5. Water GPU time

The water is drawn inside the frame's translucent passes, which belong to the frame renderer, so
the layer cannot bracket its own draw there. `WaterGpu::time_draw` draws the armed water once
more into a private `Rgba16Float` target (the viewport, each side clamped to 2048) with the frame's
depth bound, bracketed by pass timestamps, resolves and reads back. It costs the water alone (no
scene copy, so a composite tier is timed as a blend). `WaterSettings::gpu_timing = N` times every
N frames (0 off, the default; each timing waits for the GPU, so it is a diagnostic);
`WaterRenderer::measure_gpu_ms(depth, camera)` times one draw now. Stats `gpu_ms`,
`gpu_ms_pixels`, `gpu_ms_samples`, `gpu_ms_supported` (needs `TIMESTAMP_QUERY`; lavapipe has
none: the value stays 0 and `measure_gpu_ms` answers `None`).

## 6. Per-ring snap

The grid has one center (the eye snapped to two finest cells `c`) and one mesh. A ring's coarse
vertices sit at `center + n · 2^k c`, so they hopped by `2c` against the world each time the center
stepped: the shimmer of the far rings. Now boundary `j` (at `m c 2^j` from the center, `m` the
inner half in cells) is shifted to the lattice of its own ring: the eye snapped down to `2^(j+1) c`,
minus the center (`RingGrid::ring_shifts`, computed in `f64`; boundary 0, the inner square's edge,
does not move). A vertex at Chebyshev distance `ρ` between boundaries `j−1` and `j` moves by the
shift interpolated linearly in `ρ` (`RingGrid::ring_shift`; shader `ring_shift`, the node shifts
in the last ten `vec4` of the swell uniform). Rules that keep the surface whole:

- Two rings share their seam vertices at the same `ρ`, so they get bit-identical shifts: no crack.
- A shift is below `2^(j+1) c` while a ring is `m c 2^(j−1)` wide: the map `ρ → ρ + shift` has a
  slope of at least `1 − 4/m`, so it never folds for `m >= 8` (`MIN_SNAP_HALF_CELLS`); a grid with
  fewer cells does not snap its rings.
- The wave fade (`ring_cell`) and the inner square use the unshifted offset, so the near surface
  is exactly the CPU query's (the parity tests).
- Culling boxes are widened by the ring's snap pad `2^(r+1) c`.
- `WaterSettings::ring_snap` (default off) and `WaterRenderer::set_ring_snap` switch it. Off
  writes zero shifts, so `local + ring_shift(local)` is `local` exactly and the swell and the
  ocean draw the vertices they drew before the snap.

Boundary `j` now steps once per `2^(j+1) c` of eye travel (the unit test counts the steps of a
panning eye per ring).

## 8. Defaults and the frame uniform

| Switch | Default | Off means |
|---|---|---|
| `WaterSettings::underwater` | off | `misc.w = 0`, `depth.w = 0`: the uniform of the frame before the view; the surface is discarded from below; no fog pass |
| `WaterSettings::ring_culling` | off | the ring grid is one `draw_indexed` over all indices, as before |
| `WaterSettings::ring_snap` | off | zero shifts: every ring follows the inner grid's snap |
| `WaterSettings::gpu_timing` | 0 | no timed draw |
| `WeatherSea` | absent | the body's swell and ocean are the author's |

`misc.w` codes: 0 view off, 1 view on with the eye above water, body slot + 2 with the eye under
that body. The swell uniform carries the ten shift `vec4` at its end whatever the switch (zeros
when off). The test `gpu::tests::bind_layout_is_pinned` pins the water's bindings 0..=12 (the
FFT ocean's maps and uniform at 11 and 12, next to the swell at 8 and the shore at 9 and 10) in the
shader, in the layout and in the ocean's compute shader (0..=4): the underwater view adds entry
points (`vs_fog`, `fs_fog`), not bindings. `water_extras_frame::defaults_are_off_and_change_nothing`
renders the unset renderer against every switch off (under water, above it, over an ocean) and
compares bytes.

## 9. Not yet

Ribbons under water (no fog, caustics or shafts for them; § 10 covers planes only); an ocean body seen from below beyond its
rectangle; a GPU time inside the frame's own translucent pass (needs a `frame.rs` hook).

## 10. Caustics and sun shafts (ADR-2432)

`WaterLook::caustics` (`WaterCaustics { enabled (false), strength, scale, speed, chromatic (0),
shaft_steps, tier_cost }`), off by default and left out of a save while it is the default. The look
is not a frozen content format (spec 12 § 4a lists none for water): the field is additive and read
with `serde(default)`, so no version bump or migrator is needed and old files load unchanged.

- **Method.** Per metre of depth a sun ray leaves the surface point `x` with the horizontal
  displacement `r(x)` (Snell's law at the ripple slope). A receiver at depth `d` is lit by the map
  `x + d·r(x)`; flux through the surface is conserved, so its irradiance relative to a flat surface
  is the inverse footprint area, `1 / |1 + d·tr J + d²·det J|` with `J = ∂r/∂x`.
- **The tile.** A compute pass writes a 256 × 256 `Rgba16Float` tile every frame, from the same
  ripple map (two layers, the surface shader's scroll scaled by `speed`): `(tr J, det J, r − r₀)`.
  The receiver reads it with one fixed-point step to find the surface point, then evaluates the
  determinant at its own depth, so one tile serves every depth. `chromatic` shifts the red and blue
  channels' focal depth by ±15 % × `chromatic`.
- **Receivers.** What the water composites below the surface (the bed seen through it) and, with the
  eye under a surface, the scene in the fog pass. The gain is blended in by `strength`, the sun's
  share of the light, the sun's height, absorption on the light's way down and the distance below the
  surface (to the ambient by 6 m), the texel footprint (fades where the tile would alias), and the
  shore depth (the pixel's water depth, 2 to 15 cm). Blend-only frames (no scene copy) draw none.
- **Shafts.** With the eye under a body with `shaft_steps` of 8 to 16, a half-resolution compute pass
  marches the view ray (to the scene or the surface, at most 60 m) and adds the Henyey–Greenstein
  in-scatter of the tile's brightening above the ambient, dimmed by the water on both legs. The fog
  pass adds it with a bilinear read. ADR-3251: with a sun shadow map bound (`WaterRenderer::set_sun_shadow`, or the
  frame's cascade through `TranslucentLayer::bind_sun_shadow` once the frame hookup is applied) each step tests where
  the refracted beam crossed the surface against the map (casters above the water, exact) and reads the straight line
  to the sun for a caster between the step and the surface (under the water, approximate: off by
  `dq (tan θ_air − tan θ_water)` away from the caster). No map bound draws the same bytes as before.
  `shaft_accumulate` (off by default) moves the jitter per frame and averages the march in a reprojected, clamped
  history of up to 16 frames, restarted on camera cuts; `shafts_from_above` (off by default) marches from where the
  view ray meets a plane's flat level when the eye is above it and adds the result to the water's in-scatter.
- **Focus depth.** The pattern is sharpest near a quarter of the tile length (`ripple_scale` × `scale`)
  below the surface; the fade ends it by 6 m. A 1.5 m tile focuses near 0.4 m, the sea's 18 m tile near
  5 m, so shallow beds under big ripples want a smaller `scale`.
- **Tiers.** Low (Steam Deck, mobile, Desktop Low) compiles the pass out: no tile, no shaft, no
  pipeline. `tier_cost` is the lowest tier that pays for it (`high` or `ultra`).
- **One tile per frame.** The first enabled body (table order) drives the tile's `speed`; each body
  keeps its own `strength`, `scale`, `chromatic` and `shaft_steps`. The tile follows the ripple map
  only: a swell's or an ocean's long waves, which no periodic tile holds, do not lens the light.
- **Reference.** `caustics::caustic_density` counts refracted rays into receiver bins (energy
  conserving by construction); `jacobian_density_at` is the shader's estimate; `cpu_tile` is the
  tile pass's mirror, compared with the GPU's readback on lavapipe.
- **Agent.** `water.set_caustics`, `water.caustics_stats` (book/agents/terrain-water.md).

## 11. Sources

Pierson and Moskowitz 1964 (fully developed sea); Gribb and Hartmann 2001 (frustum planes);
Akenine-Möller, Haines, Hoffman, *Real-Time Rendering* (box-plane test); Snell's law and Schlick's
approximation of the Fresnel term; Beer–Lambert attenuation; Losasso and Hoppe 2004, geometry
clipmaps (per-ring snap, § 6); for § 10, Wallace, Cohen and Greenberg 1987 (light-ray density),
Guardado and Sanchez-Crespo, GPU Gems ch. 2 (caustics from wave slopes) and the Henyey–Greenstein
phase function (shafts). Rows in `docs/provenance.md`.
