# Cloud types: genera, diffuse light, cirrus

The volumetric cloud layer ([clouds](clouds.md)) has four genus presets (ADR-1408,
`regia_render::cloud_types`). Each one sets the layer's base and top heights, extinction,
coverage, cloud type, noise tiles and a `CloudLook`. The numbers come from the WMO International
Cloud Atlas (heights) and from measured liquid water content and droplet size, `β = 3 LWC / (2 ρ r)`:

| Genus | Name | Base–top (m) | Extinction (1/m) | Coverage | Look |
|---|---|---|---|---|---|
| Cumulus humilis | `fair_cumulus` | 1200–2300 | 0.04 | 0.22 | small, flat, well separated |
| Cumulus mediocris–congestus | `cumulus` | 1200–4800 | 0.075 | 0.38 | tall heaps, deeper erosion |
| Broken stratocumulus | `stratocumulus` | 900–1900 | 0.045 | 0.66 | a low sheet of cells with gaps |
| Cirrus fibratus / uncinus | `cirrus` | sheet at 9000, few humilis at 1300–2200 | τ 0.2 (sheet) | sheet 0.5 | thin ice streaks along the wind |

`CloudLook` holds what the presets change in the march. `CloudLook::LEGACY` is the default, so a
layer that never sets it renders the same pixels as before:

- `diffusion`: sunlight scattered many times inside the cloud, as a two-stream diffuse field. Its
  transmission falls as `1 / (1 + ¾ (1 − g) τ)` (Bohren 1987), not `exp(−τ)`. With physical
  extinction the sun-ward optical depth in a cumulus is tens, so with the old exponential octaves
  only the sky lit the cloud and every cloud was one flat grey. About 2 is physical. With
  diffusion on, the view march also keeps its step inside a cloud under two optical depths, and
  a long step that enters a dense cloud steps back, so the lit rind is sampled.
- `erosion`: how deep the detail noise bites (0.5 before).
- `breakup`, `breakup_scale_m`: a finer coverage noise that breaks the field's edges into lobes
  and cells.
- `cirrus` (`CirrusSheet`): coverage, altitude, vertical optical depth, streak width and stretch.
  The sheet is single-scattering with a forward ice phase function (g ≈ 0.75). Its streaks run
  along the wind and drift 2.5 times faster than the low layer. Aerial perspective is applied in
  front of it. It draws only where the footprint sees sky, behind the volumetric layer.
  The streaks are fallstreak trails (ADR-1470): generating cells at random points drop a dense,
  sometimes hooked head and a soft trail that widens and fades downwind, with fibres along it.
  Trails come in groups with clear sky between them; a broad warp turns neighbouring groups a
  little, so the sky holds no grid of parallel strands. `cirrus_scale_m` is the cell width across
  the wind and `cirrus_stretch` the cell length along it (in widths). Detail finer than a few
  pixels fades to its mean (the trail profile widens, the fibres fade), so the sheet turns into
  a soft veil toward the horizon instead of hair. The CPU mirror is
  `regia_render::cloud_types::cirrus_density`.
  Six optional values make the sheet fibrous (combed strands, sheared hanging tails, hooks,
  patchy breakup; ADR-1509). They are all off by default; see [Fibrous cirrus](cirrus-fibrous.md).
  `crisp` (ADR-1516, off by default) sharpens them; see [Crisp cirrus](cirrus-crisp.md).

## Agent

- `render.set_clouds { genus: "stratocumulus" }` applies a preset. `diffusion`, `erosion`,
  `breakup`, `breakup_scale_m`, `cirrus_coverage`, `cirrus_altitude_m`, `cirrus_optical_depth`,
  `cirrus_scale_m` and `cirrus_stretch` then change single values (clamped).
- `render.get_clouds` reports `look { diffusion, erosion, breakup, breakup_scale_m, cirrus {...} }`,
  `shape_scale_m`, `detail_scale_m`, `genus` (the preset the layer holds exactly, else null) and
  `genera`.
- The same names are weather presets: `render.set_weather { preset: "cumulus", transition_s: 20 }`
  eases the layer's heights, tiles and look along with its coverage. When you go to or from a
  preset without a genus (`fair`, `cloudy`, …), the genus's layer holds for the whole
  transition. `render.get_weather` reports each state's `genus`.
- Errors: `F0002` (`-32602`, invalid params) for an unknown genus, non-string genus, or a non-finite
  numeric look value.

Captured on lavapipe (llvmpipe Vulkan): an unknown genus is refused without changing the cloud layer.

```json
{"jsonrpc":"2.0","id":4,"method":"render.set_clouds","params":{"genus":"nimbus"}}
{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"unknown cloud genus `nimbus`; one of fair_cumulus, cumulus, stratocumulus, cirrus","data":{"code":"F0002","hint":"check the method's parameters in the agent guide"}}}
```

Workflow: read `render.get_clouds`, apply one genus with `render.set_clouds`, then capture a
`seeing.screenshot` after the next frame. Tune a single `diffusion`, `erosion` or cirrus field and
capture again; use `clear_look_override: true` to clear previous weather overrides (look fields in
the same call become new overrides).

```rust
use regia_render::{CloudGenus, CloudSettings};

fn stratocumulus() -> CloudSettings {
    CloudSettings::default().with_genus(CloudGenus::Stratocumulus)
}
```

Cost: the cirrus sheet is three texture taps per sky texel at the march resolution, plus at most
six hashed generating cells where a band covers the texel, once per texel however many marches
the profile averages (ADR-1470). Diffusion
replaces two exponentials with one division. The step cap adds steps only inside dense cloud,
where rays end within a few steps. Measured numbers are in the lane report (showcase-lookdev-1).
