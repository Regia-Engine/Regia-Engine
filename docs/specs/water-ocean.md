# FFT ocean (N19, ADR-1969)

A wind sea on a plane water body: a directional spectrum evolved and inverse-transformed by a compute
pass into tiling maps. The drawn surface on High and Ultra, the swell on Low, one CPU answer for gameplay
on every tier. Builds on `specs/water.md` (ADR-1620 swell, ADR-1399 frame seam) and spec 87 (shore field).

## 1. Authoring

`WaterBody::ocean: Option<FftOcean>` on a plane (`#[serde(default)]`: older bodies have none, no format
bump; `WaterBody` is not in a frozen content format). A ribbon refuses one (`OceanNeedsPlane`).

| Field | Range | Default | Meaning |
|---|---|---|---|
| `seed` | u32 | 1 | Random phases. Same seed + same clock = same sea, on any machine |
| `spectrum` | `jonswap`, `phillips` | `jonswap` | § 2 |
| `wind_speed` | 0.5..=40 m/s | 12 | Wind 10 m above the water |
| `wind_direction` | non-zero `[x, z]` | `[1, 0]` | Direction the wind blows toward |
| `fetch` | 1 000..=2 000 000 m | 300 000 | Open water the wind has crossed (JONSWAP) |
| `gamma` | 1..=7 | 3.3 | JONSWAP peak enhancement |
| `spread` | 1..=64 | 8 | `s` of `cos^(2s)(θ/2)`: larger is more long-crested |
| `choppiness` | 0..=2 | 0.9 | Horizontal offset `λ` (0 height field, 1 linear particle motion) |
| `height_scale` | 0..=4 | 1 | Scale on the heights |
| `tiles` | 8..=20 000 m | `[1009, 257, 67]` | Band tiles, longest first, each ≥ 2 × the next |
| `crest_foam`, `foam_onset` | 0..=4, 0..=1 | 1, 0.15 | Foam from the Jacobian, as the swell's |
| `shore_start`, `shore_end` | `shore_end`..=1000 m, 0..=1000 m | 24, 3 | Depths of the shore blend (§ 6). `shore_start` below `shore_end` is rejected (ADR-5083) |
| `grid` | 0, 128, 256, 512, 1024 | 0 | Texels per side of the drawn maps; 0 lets the tier choose (§ 3, ADR-2126) |
| `foam_decay` | 0..=120 s | 0 | Lingering foam: seconds full foam takes to fade; 0 keeps foam instantaneous and allocates nothing (§ 7a, ADR-2843) |
| `foam_growth` | 0..=30 s | 0.5 | Seconds a breaking crest takes to leave about two thirds of its foam behind (§ 7a) |
| `whitecap_rescale` | bool | false | Rescale the drawn Jacobian by the slope variance the faded bands lost (§ 7a) |

`WaterBody::parked_ocean: Option<FftOcean>` holds the ocean while `WaterBody::set_wave_mode(WaveMode::Swell)` has the
plane on its swell (§ 8). `#[serde(default)]`, and left out of the output when empty, so a body that never switched
serializes as before.

## 2. Spectrum

`a(k)` is the amplitude of the wave running along `+k`; `h(x, t) = Σ ĥ(k, t) e^(ik·x)` with
`ĥ(k, t) = a(k) e^(−iωt) + conj a(−k) e^(iωt)`, `ω = sqrt(g |k|)` deep water.

- `a(k) = ξ sqrt(E(k) Δk² / 2)` with `⟨|ξ|²⟩ = 1`, so the two waves of a pair carry the variance
  `E Δk²`; `E` is the variance density over the wave-vector plane, `∫ E d²k = m₀`, `Hs = 4 sqrt(m₀)`.
- JONSWAP: `S(ω) = α g² ω⁻⁵ exp(−5/4 (ω_p/ω)⁴) γ^r`, `r = exp(−(ω−ω_p)² / (2σ²ω_p²))`, `σ` 0.07 / 0.09,
  `α = 0.076 (U²/(F g))^0.22`, `ω_p = 22 (g²/(U F))^(1/3)`; spread `D(θ) = Q cos^(2s)(θ/2)`;
  `E = S(ω) D(θ) (dω/dk) / k`.
- Phillips: `E = (α/2) k⁻⁴ exp(−1/(k L)²) (2/π) cos²θ` over the downwind half, `α` 0.0081, `L = U²/g`.
- `ξ` is hashed from the seed, the band and the integer mode `(nx, nz)`, never drawn from a stream:
  a mode has the same amplitude at every map size.

## 3. Bands and tiers

Three tiles. Band `i` holds `lo_i ≤ |k| < hi_i`, where a band's short end is its tile at four texels
per wavelength on the 128 grid (`OceanTables::band_limits`) and the last band is open. Every mode is in
exactly one band; the mean and the Nyquist row and column stay empty (they have no conjugate).

| Tier | Maps | What it draws |
|---|---|---|
| Low (Deck, mobile, Desktop Low) | none, nothing dispatched | the swell: the body's, or a stand-in fitted to the spectrum (§ 6) |
| High | 3 bands × 128² | every mode the queries use |
| Ultra | 3 bands × 256² | the same modes, plus finer ones in the last band |

`FftOcean::grid` overrides the size on High and Ultra: 128, 256, 512 or 1024 (ADR-2126). Low still dispatches
nothing, and queries still use the 128 grid (§ 5), so the grid changes the look and the cost and never what gameplay
feels. A grid the device has no limits for (`grid_fits`: buffer and binding size, texture size, workgroups) drops
the ocean to its swell, with one warning per grid, and it counts in `ocean_dropped`. Any other value fails
`validate`, and like any invalid body the frame does not draw it (`build_mesh`), so it never reaches the transform.

The band limits use the 128 grid at every size, so a larger grid differs from 128 only by the finer modes in the last
band, down to 13 cm waves at 1024 with the default tiles. The long bands keep their modes and gain only sampling: at 1024
they have 32 texels per wavelength at their short end. A
body with `ocean_draw` off, a device without the compute limits (`OceanGpu::ready`) and any body after
the frame's first ocean spectrum draw like Low. **One spectrum per frame**: bodies with an equal `FftOcean`
share the maps, others are dropped to their swell and counted (`ocean_dropped`). The count holds every ocean body
that draws its swell instead: a different spectrum, a grid the device cannot hold, and all of them on a frame that
draws no ocean (Low, `ocean_draw` off, no compute path) (ADR-5826).

## 4. Compute pass

`water_ocean.wgsl`, in storage buffers (no storage-texture format): per band one spectrum dispatch and
`2 log2 N` Stockham radix-2 stages, ping-ponging two buffers, then one copy into an `Rgba32Float` array
of two layers per band: layer 0 `(Dx, h, Dz, Dxz)`, layer 1 `(hx, hz, Dxx, Dzz)`. Each vec4 is two complex
numbers while the transform runs: the spectrum `A + i B` of two real fields gives A and B in the real and
imaginary parts.

Registered as a frame extension (ADR-1552): `WaterRenderer::ocean_pass`, name `water_ocean_fft`, slot
`after_opaque`; `WaterPlugin` registers it with the first ocean body, so a frame without an ocean has no
extension. It shows in `render.graph`. The slot does not run on a display-only frame, and a host that
drives `FrameRenderer` itself may not register it: the water layer's `simulate`, which runs on the same
encoder before the water draws, then records the same evolve. The evolve is idempotent per instant, so a
frame does it once whichever path comes first. `frame.rs` is not touched.

## 5. Determinism and the CPU mirror

- `ω` is a whole multiple `n` of `2π / T`, `T` = 2048 s. The phase of a mode is the integer `n τ` modulo
  2³², with `τ` the loop fraction in 2⁻³² (`loop_fraction`): the same integer in the shader and on the
  CPU, after any number of hours. The sea repeats exactly every `T` seconds.
- `OceanFrame::at` is the mirror: the same modes, the same fields (plus particle velocity), by an inverse
  FFT on the CPU at 128². `water.surface`, `WaterBody::surface` and `raycast` use it on every tier,
  whatever is drawn, so gameplay does not depend on the graphics settings. High draws exactly this
  surface; Ultra and larger grids add the finer modes.
- `OceanFrame::at` runs at any of `OCEAN_GRIDS`, so it is also the reference for tests: the GPU maps match it
  field by field at every grid, for both spectra, within 1 mm on lavapipe
  (`gpu_maps_match_the_cpu_reference_at_every_grid`), and it matches the direct sum over the modes at the large
  grids (`reference_fft_matches_the_direct_sum_at_the_large_grids`).
- A frame is built by the first query of a tick and shared by the rest (`frame_at`, a small memo keyed
  by the `FftOcean`, the size and `τ`; a pure function, so the memo changes only the time).
- `sample` blends texels as the shader does: bilinear with wrap-around, so a band tiles on its own tile
  and the sum repeats at the common multiple of the tiles.
- The rest point comes from the same Newton steps as the swell (`swell::rest_point_by`).
- Foam: `crest_foam(J)` of the ocean's Jacobian, instantaneous: a pure function of the tick. Lingering foam
  (§ 7a) is drawn only; the query's `crest_foam` stays the instantaneous one, so it never depends on the
  frames drawn before.
  The default onset is 0.15: `1 − J` of this sea has a deviation near 0.12 at 14 m/s, and the mean foam
  is then about 1 % of the surface, more with more wind (`default_foam_covers_a_little_of_a_windy_sea`).

## 6. Near the shore

A deep-water spectrum means nothing where the bed reaches the waves, and a tiling map cannot shoal. Over
a shore field the surface is `w F + (1 − w) S`, with `w` a smoothstep of the depth at the rest point from
`shore_end` to `shore_start`, `F` the ocean, and `S` the shoaled swell of spec 87. Without a field,
`w = 1`. `S` is the body's swell, or `FftOcean::stand_in_swell`: `Swell::open_sea` downwind with the
spectrum's `Hs` and peak wavelength, so an ocean body needs no authored swell. Deep water draws no swell.

The Jacobian foam blends by `w`, the breaker foam by `1 − w`; shore foam is unchanged.

## 7. Drawing

Ring grid as for a swell plane (a plane with an ocean carries the stand-in if it has no swell).
`ring_world` adds `w · ocean_offset`, each band faded on the grid's cell by `wave_fade(cell, rms_k)` of
the band's energy-weighted wave number. The fragment adds `w` times the slope and the Jacobian foam, each
band faded on the pixel's footprint, and what a faded band lost goes to the GGX roughness as mean square
slope. `textureLoad` plus a bilinear blend: the same texels as the CPU, no `FLOAT32_FILTERABLE`.

Binding 11 is the map array, binding 12 the ocean uniform; `SwellBody::level.z` says a body rides the
ocean. A 1 × 1 stand-in and zero bands when no ocean is drawn: frames without an ocean run the same math
as before (the weight is 0, `(1 − 0) · x` is `x`).

## 7a. Lingering foam and whitecaps with distance (ADR-2843)

Both are opt-in and off by default; an ocean with neither allocates, dispatches and draws exactly what it did
before (`ocean_foam.rs`).

**Lingering foam** (`foam_decay` > 0). Each band keeps a history map on its own tile, one value per texel, in
the tail of the first work buffer (no FFT stage writes there) and three more layers of the map array (layer
`2 · bands + band`). After the FFT, `cs_foam` steps it once per evolve:

- injection `c = smoothstep(onset, onset + 0.3, s_b (1 − J_b))`, `J_b` the band's own Jacobian, `s_b` one over
  the band's share of the deviation of `1 − J` (`sqrt(σ_b² / Σ σ²)` of the slope variances, at least 0.05), so
  every band crosses the onset at the number of deviations the summed surface does (`band_foam_scales`);
- growth `H ← H + (c − H)(1 − exp(−Δt / foam_growth))` where `c ≥ H`, linear decay `H ← max(c, H − Δt / foam_decay)`
  otherwise: foam a crest left reaches exactly zero `H foam_decay / Δt` steps after it has gone (`foam_step`).

`Δt` is the water clock's step since the last evolve. The history restarts from no foam on a step back, a step
longer than 1 s (`FOAM_MAX_STEP`), a new simulation (another ocean or grid), a change of the bodies, a camera cut
(`WaterRenderer::set_camera_cuts`, which `WaterPlugin` feeds with the frame renderer's cut count, so a cut the
renderer detects while drawing restarts it on the next frame) and `WaterRenderer::reset_ocean_foam`. The maps
are indexed by the rest point, so the history moves with the water's orbital motion without an advection step.

The fragment draws `max(crest_foam(J), strength · max_b(fade_b H_b))`. The CPU mirror is `FoamHistory`
(`step`, `sample`), and `OceanFrame::drawn_foam` mirrors the fragment; the GPU history matches the mirror within
2e-3 on fixed steps (`history_matches_the_cpu_mirror_on_fixed_steps`).

Cost: one dispatch per band and `2 × 3 × N² × 16` bytes, 1.6 MB at 128, 6.3 MB at 256, 25 MB at 512, 100 MB at
1024 (`OceanCost::history_bytes`, `history_dispatches`). The first work buffer is bound whole, so at 1024 its
binding grows from 101 MB to 151 MB, past wgpu's default storage-binding limit of 128 MiB: `grid_fits` counts the
tail, and a device with only the default limits draws a 1024 ocean with lingering foam as swell (`ocean_dropped`).

**Whitecap rescale** (`whitecap_rescale`). A band faded on the pixel's footprint takes its share of `1 − J`
with it, so whitecaps thin out with distance. The rescale scales the drawn compression back up by the
deviation the faded bands lost: `1 − J' = r (1 − J)`, `r = sqrt(Σ σ_b² / Σ fade_b² σ_b²)`, at most 4
(`rescaled_jacobian`), under the Gaussian model of the Jacobian's spread used for whitecap coverage by Dupuy
and Bruneton (2012). On the 14 m/s default sea the drawn coverage keeps within the bound
`whitecap_coverage_holds_with_distance_when_rescaled` states out to 8 m pixels; without it, it falls below half the near coverage (ADR-2843 has the numbers).

**Queries on Low** (`WaterSettings::queries_follow_tier`, off by default). With it on, surface and ray
queries answer the surface the tier draws (`WaterBody::drawn_for`): on Low, or with `ocean_draw` off, an ocean
plane answers its swell or the ocean's stand-in, and a swell answers the waves the tier draws. A floater on Low
then rides the drawn vertices within 5 mm near the camera (`a_floater_on_low_rides_the_drawn_surface`). It stays
off by default because the rule of § 5 holds: gameplay must not depend on a graphics setting, or two machines of
one networked game disagree about where a boat is. With it on, the query also follows the renderer's own fallbacks
(ADR-3251): `WaterRenderer::oceans_drawn_into` says per body whether the frame drew its ocean, the plugin keeps the
bodies it left to their swell in `WaterDrawnOceans` (a grid the device's limits refuse, named by `OceanGridError` from
`check_ocean_grid`; a second spectrum), and `WaterBody::drawn_with(tier, ocean_drawn)` builds what was drawn. A
per-body `DrawnWaterCache` (rebuilt when the tier, that answer or the body changes) keeps steady queries free of
allocations (`follow_tier_queries_allocate_nothing_for_the_drawn_bodies`,
`queries_follow_the_renderers_ocean_fallback_on_a_128_mib_device`).

## 8. Agent

`water.set { ocean, ocean_draw }`, `water.state` (`bodies[].ocean`, `bodies[].mode`, `bodies[].parked_ocean`,
`ocean_draw`, `last_frame.ocean_*`), `water.ocean { entity? }`, `water.ocean_check`, and (ADR-2126)
`water.set_mode { mode, entity?, grid?, ocean? }` and `water.ocean_cost { grids?, frames?, entity? }`. Book:
`book/agents/water-ocean.md`.

**Modes.** `WaterBody::set_wave_mode`: `fft` brings the parked ocean back, or the default one; `swell` parks the ocean, and
drawing and queries go back to the swell. A body with no swell takes the ocean's stand-in while parked, and gives it up
on the way back if nobody changed it, so a round trip restores the body exactly and the frame byte for byte
(`switching_back_to_the_swell_restores_the_frame_bytes`). A ribbon refuses (`OceanNeedsPlane`). The mode is
per body; `ocean_draw` stays the global switch for a side-by-side look, which leaves queries on the ocean.

## 8a. The maps as outputs

The array holds three cascades, one per band, of the displacement and of what the normal and foam are made from. There is
no separate foam channel, since foam is a function of three stored fields:

| Output | Channels | Accessor on `OceanMaps` |
|---|---|---|
| Displacement | layer 0 `Dx, h, Dz` | `displacement` |
| Normal | layer 1 `hx, hz` with layer 0 `Dxz` and layer 1 `Dxx, Dzz`: `(∂P/∂z) × (∂P/∂x)` | `band_sample(..).normal()` |
| Foam | `crest_foam(J)`, `J = (1 + Dxx)(1 + Dzz) − Dxz²` | `band_foam` |

A larger grid foams more at the same onset, since finer waves compress the surface more: at 14 m/s the share of the surface
past the onset, unfaded, is 10.5 % at 128, 13.2 % at 256, 15.3 % at 512 and 16.9 % at 1024 (`foam_share_by_grid`). Nothing
compensates for this: a host that raises the grid and wants the same whitecaps raises `foam_onset`. The bands fade with
distance in the fragment shader, so far water sees less of the increase.

The fragment shader takes `J` of the sum of the bands, so the drawn foam is not the sum of `band_foam`. On a real GPU at
512² the slopes and the Jacobian agree with central differences of the displacement within 5 % of their peak
(`normal_and_foam_maps_follow_the_displacement`).

## 8b. Cost

`WaterRenderer::measure_ocean_cost` runs `frames` evolves on a simulation of its own, so the frame's maps stay as they
were, and reports the compute pass and the copy by encoder timestamps where the device has them, wall time from submit
to done, modes, memory, and the once-per-grid set-up. 

| Grid | Dispatches | Modes | GPU memory | Compute | Copy | Frame against the swell | Set-up, once |
|---|---|---|---|---|---|---|---|
| 128 (High) | 45 | 22 104 | 6 MB | 0.09 ms | 0.02 ms | +0.10 ms | 18 ms |
| 256 (Ultra) | 51 | 71 000 | 25 MB | 0.11 ms | 0.06 ms | +0.17 ms | 47 ms |
| 512 | 57 | 267 096 | 101 MB | 0.25 ms | 0.19 ms | +0.50 ms | 141 ms |
| 1024 | 63 | 1 052 504 | 403 MB | 1.10 ms | 0.75 ms | +1.84 ms | 607 ms |

RTX 5090 (one card, driver 615.71.09, Vulkan, P0), 1920 × 1080, 2026-11-02. Compute and copy are medians by GPU timestamps
over five turns of 64 evolves; a turn repeats to within 0.5 %. The frame column is the frame's GPU time against the same
body drawing its swell, medians of 80 frames; two identical 128 setups differ by 0.02 ms, which is the resolution here. The scene
is small (0.4 ms on its own), so read the column as what the ocean adds, not as a share of a real frame. Set-up is CPU time for the
tables and the upload, on a host at load 75 at nice 15, so an upper bound. At 1024 it is a visible hitch on the first frame.

Compute grows slowly at the small grids (128 to 256: 1.26 times, for 4.6 times the work) and at about the work's rate above 512
(4.45 times). That fits a fixed cost near 2 µs per dispatch at small grids and a higher cost per texel once the maps outgrow
the caches, but only the totals were measured, not those two terms. The maps match the CPU reference on this driver as they do on
lavapipe: worst field error 2.2e-6 m at 1024, ticks 240 and 86 400.

Memory at a grid is four times `3 × 2 × N² × 16` bytes (modes, two work buffers, the array): 25 MB at 256, 100 MB at 512,
400 MB at 1024. Setting a grid up is once per ocean and grid, on the frame that first needs it; at 1024 that means a
million modes, so a host should expect a hitch on that frame (§ 9).

## 9. Out of scope

Pre-warming a large grid off the frame (the first frame at 1024 pays for the tables and the upload);
mip-mapped, filtered maps (Rgba16Float) for far normals and far foam (the drawn Jacobian loses each band
with the band; `whitecap_rescale` restores the coverage, § 7a, not the shape); depth-limited spectra (TMA);
wave refraction; more than one spectrum per frame; a swell on the ocean's own stand-in that stays
coherent with the ocean on Low (by default a floater on Low rides the ocean's surface, which Low does not
draw; `queries_follow_tier` makes it ride the stand-in instead, § 7a); lingering foam on the swell; foam
advected by currents (`flow`).
