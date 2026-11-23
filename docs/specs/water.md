# Water: bodies, swell and surface queries

Status: proposed with ADR-1620. Supersedes the water bullet of `26-terrain-vfx-highend.md` § 1
("planar water … no simulation") and records the runtime-frame state of ADR-1399. Crate:
`regia_terrain::water_body` (runtime) and `regia_terrain::water` (the offscreen shoreline plane,
ADR-1237). Agent page: `book/agents/water.md`.

## 1. What exists (ADR-1399, ADR-1237)

- **Bodies.** A `WaterBody` component is a level rectangle (`WaterShape::Plane`: a lake or the
  sea) or a sloped ribbon along a centerline (`WaterShape::Ribbon`: a stream or a river, speed by
  Manning's equation), with a `WaterLook` (Beer–Lambert absorption, scattering, roughness,
  ripples, refraction, wet band, foam, glint), obstacles that raise standing waves and an
  optional ice shelf. At most 16 bodies per frame.
- **Frame.** `WaterPlugin` draws them through the frame renderer's translucent-layer seam, in
  the ECS runtime, render-out and offscreen tests. Tiers: Low (Steam Deck, mobile, Desktop Low:
  blended, no scene copy), High and Ultra (refraction, screen-space reflection, standing waves).
- **Clock.** Every animated term is a pure function of the fixed-step `WaterClock` tick, reduced
  in `f64`; the same tick renders the same image.
- **Shoreline plane.** A `Terrain` may carry a `WaterPlane` (part of the frozen terrain format).
  The offscreen terrain renderer shades it (ADR-1237). In the runtime frame, `WaterPlugin`
  draws it as a plane body (§ 2).

## 2. Terrain water planes in the runtime frame (ADR-1620)

For each `Terrain` with a `WaterPlane`, `WaterPlugin` keeps one runtime `WaterBody` on its own
entity tagged `TerrainWater { terrain, source, world_width }`: a rectangle over the whole map
(centered on the origin, `world_width` on a side) at the plane's level. Its look is derived
(`look_from_plane`): extinction `6 / max_depth` (the optical depth ADR-1237 reaches at
`max_depth`), half of it scattering and, per channel, absorption `σ (1.2 − shallow_color)`;
in-scatter albedo `deep_color`; wet band `foam_width`; ripple tile `8 · 2π / wave_frequency`
and ripple strength in proportion to `wave_amplitude · wave_frequency`. A change to the plane
resets the body's shape and look and keeps its swell, obstacles and ice; no plane, no body.
`WaterSettings::terrain_planes` (agent `water.set { terrain_planes }`) switches them all. No
format changes.

## 3. Swell (ADR-1620)

A plane body may carry a `Swell`: up to 32 `SwellWave`s and the crest-foam settings.

| Field | Range | Meaning |
|---|---|---|
| `wavelength` | 0.25..=2000 m | crest to crest; wave number `k = 2π / λ` |
| `height` | 0..=40 m | crest to trough; amplitude `A = height / 2` |
| `direction` | non-zero `[x, z]` | travel direction, normalized when used |
| `steepness` | 0..=1 (0.5) | horizontal over vertical amplitude, `H = steepness · A` |
| `phase` | −10 000..=10 000 (0) | radians (ADR-5826: the bound keeps an `f32` phase to under a milliradian) |
| `crest_foam` | 0..=4 (1) | crest foam strength |
| `foam_onset` | 0..=1 (0.15) | horizontal compression `1 − J` where foam starts |

- **Surface.** A point resting at `r` on the still plane moves to
  `r + Σ (H d cos θ, A sin θ)`, `θ = k d · r − ω t + φ`, `ω = √(g k)` (deep water). The fold sum
  `Σ steepness · k · A` must be at most 1 (else `SwellFolds`); only planes carry a swell
  (`SwellNeedsPlane`); at most 32 waves (`TooManyWaves`).
- **One function, two places.** The CPU (`swell.rs`) and the shader (`water_body.wgsl`) evaluate
  the same terms in the same order. The phase `−ω t + φ + k d · origin` is reduced modulo 2π in
  `f64`; only `k d · (r − origin)` is formed in `f32` (the GPU's origin is the ring grid's
  center, the CPU query's the query point), so no large argument reaches a sine at any distance
  or session length. Tests pin the shared constants by text.
- **Queries.** `WaterBody::surface(x, z, clock)`: the rest point under `(x, z)` by eight Newton
  steps on the horizontal map (a fixed-point step where its Jacobian determinant is below 0.05),
  then height, normal (`∂P/∂z × ∂P/∂x`), velocity (`∂P/∂t` plus drift) and crest foam. It uses
  every wave, whatever the tier draws. `WaterBody::raycast(origin, direction, max, clock)`: the
  ray clipped to the body's box (rectangle widened by `Σ H`, level ± `Σ A`), 128 equal march
  steps on the height above the surface, 24 bisections of the first sign change. Agent methods
  `water.surface` and `water.raycast`.
- **Crest foam.** `strength · smoothstep(onset, onset + 0.3, 1 − J)` with `J` the determinant of
  the horizontal map's Jacobian, then the look's `foam` and the fine noise as for whitewater.
- **Ring grid.** A swell plane is not in the static mesh; it is drawn on a camera-centered ring
  grid built in offsets from its center: an inner square of `2M` cells of `c` meters, then rings
  whose cells double, each reaching twice as far, zipped 2:1 at each ring's inner edge (no
  T-junctions). `c` spans 6 pixels at the nadir (`6 · 2 h tan(fov/2) / rows`), rounded down to a
  power of two in 1/32..=64 m; `M = ⌈24 · tan 30° / tan(fov/2)⌉` (even), capped by the tier
  (Low 16, High 32, Ultra 48); rings until the body's far side or the far plane (at most 20). The
  center is the eye snapped to `2c`; the shader clamps each vertex to the body's rectangle and
  fades each wave out of the displacement from four to two vertices per wavelength, with the cell
  `c · max(1, 2 d / (M c))` at Chebyshev distance `d` (continuous across rings). The fragment
  normal fades each wave by the pixel footprint the same way. The mesh is rebuilt only when the
  grid or the swell bodies change.
- **Tiers.** At most 8 (Low), 16 (High), 32 (Ultra) waves per body, the first in the list; the
  frame holds at most 128 waves for all bodies (the rest are counted as dropped).

## 4. Acceptance

- Constants and uniform layout pinned between Rust and WGSL (unit tests).
- CPU surface: closed form for a sine, derivatives against finite differences, the inverse lands
  within 0.1 mm, precision far from the origin and after 200 000 periods (unit tests).
- GPU/CPU parity: the vertex shader's placement (compute probe on the same function) equals the
  CPU mirror within 1 mm, and near the camera the CPU surface query within 1 mm (lavapipe test;
  RTX 5090 run by the lead).
- Hash-stable frames per tick; the swell moves between ticks; tier wave caps (lavapipe test).
- Steam Deck frame time (ADR-5826, measured 2026-11-11 on RADV VANGOGH, Mesa 26.0, 1280×800, release
  `water_swell_shots --perf --frames 48`, two runs within 0.1 ms): the swell on the Deck's Low tier costs
  1.0–1.5 ms of GPU time over the same sea flat (boat 1.5, cliff 1.4, scope 1.0 ms), 4–6 % of the 25 ms frame of
  the Deck profile's 40 fps cap. High and Ultra, which the Deck does not use, cost 5.0–7.9 and 7.4–17.4 ms there.

## 5. Not yet

Shoaling and breaking at the shore, the shore distance bake, floating bodies and one surface query
over every body kind are in `docs/specs/87-water-finish.md` (ADR-1828), the FFT ocean band (N19) in
`docs/specs/water-ocean.md` (ADR-1969); the underwater view, ring frustum culling and per-ring
snap, the sea-state table driven by the scene wind and the water draw's GPU time are in
`docs/specs/100-water-extras.md` (ADR-2342; all off by default). Still not built: outline-shaped
lakes, a GPU time taken inside the frame's own pass.
