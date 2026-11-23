# Daylight calibration: grey card, cirrus and lit smoke

Outdoor light in the engine is physical: the sun is the atmosphere's transmitted illuminance (lux),
the sky is the sky-view table's radiance (cd/m²), and the HDR target holds the same units. So "the
ground looks too dark against the sky" is a question with a number for an answer. This page covers
the tools and the shading that ADR-1425 added after the showcase's first look round.

## The grey card

`regia_render::daylight` lays an 18 % grey card on open ground and computes what it shows:

```text
L_sun   = ρ / π · (E_sun · sin(el) + E_sky)
L_shade = ρ / π · E_sky
```

`GreyCard::engine(&luts, albedo)` uses the engine's own sun (`transmitted_sun`) and the cosine-weighted
sky (`ambient_irradiance_table`), the terms that light terrain and meshes. `GreyCard::ies(el, albedo)`
uses the IES clear-sky daylight model (IES Calculation Procedures Committee 1984: direct normal
`127.5 klx · exp(−0.21 · m)`, diffuse horizontal `0.8 + 15.5 · sin(el)^0.5` klx). `metered_card` runs
the auto-exposure meter (the 40th–95th percentile log average, mapped to middle grey 0.18) over a
frame that is part sky and part ground, and returns where the card lands.

Measured at the showcase's 21° sun, with its air (aerosol optical depth 0.18, Ångström 1.3):

| | engine | IES clear sky |
|---|---|---|
| direct normal | 84.9 klx | 71.0 klx |
| direct on the ground | 30.4 klx | 25.4 klx |
| sky on the ground | 12.5 klx | 10.1 klx |
| card in sun | 2,460 cd/m² | 2,035 cd/m² |
| card in shade | 717 cd/m² (0.29 of sun) | 577 cd/m² (0.28) |

The clear sky 30° up and 90° from the sun is 2,780 cd/m², so a sunlit 0.18 card is 0.88 of the sky
beside it, and dead grass (albedo about 0.2) is about equal to it. The meter puts the card at 0.17
(ground only) or 0.15 (a quarter of the frame sky): within a stop of middle grey.

The default Earth air (aerosol optical depth 0.005, almost none) gives half the IES sky light, so shade
is only 0.15 of sun. A map that wants realistic clear-day shadows sets `aerosol_turbidity` 0.1–0.2 in
its `sky.ron`.

### What made the showcase ground dark

The look-round-1 ground was 13 times darker than the sky. The light was right (above); the content
was not. A terrain layer's `albedo` is a **tint that multiplies its texture**. The showcase graded its
textures to their target albedo (dead grass 0.21) and also wrote that albedo as the tint (0.36, 0.30,
0.20), so the ground rendered at about 0.07, and the mud at 0.006. With the tints at 1 the ground
median goes from 13× to 3.9× below the sky (display values; see the lane report for the rest).

`regia_terrain::layer_albedo_report` gives each layer's tint, texture mean and effective albedo, and a
warning when the effective albedo is darker than any natural ground (0.03) or when the tint itself
looks like an albedo on a texture that already has one. `TerrainSharedDraw::set_material` logs the
warnings and stores the report in the draw's `GroundLookHandle`.

## Cirrus

A cirrus sheet at 9 km hides only the air *behind* it: in a stratified atmosphere the fraction of the
air above height `h` along any upward ray is `exp(−h / 8 km)`, about a third at 9 km. The air in front
still reaches the eye. A streak adds its own light (the sun at the sheet's height through the ice phase
function, plus the sky above and the light from below) and removes a third of the sky's in-scatter, so
it is brighter than the sky around it. The ice phase function is two Henyey–Greenstein terms (a sharp
forward lobe for diffraction, g 0.9, weight 0.7, and a broad one, g 0.4; mean asymmetry 0.75).

`cloud_types::cirrus_composite` is the CPU mirror. A full streak 30° above the horizon at the showcase
sun is 2.6× the local sky 28° from the sun, 1.7× at 54°, 1.5× at 80° and 1.2× at 130°.

The shape is thin filaments, not a veil with gaps: three octaves of noise stretched along the wind and
3–16 times across it, each turned into a thin ridge where it crosses its middle (`cirrus_ridge`),
curl-warped into hooks, faded in and out along the wind, and gathered into bands that cover about
`coverage` of the sky.

## Lit smoke and dust

Lit puffs (`RenderModule::lit`) under a frame with a sky are shaded as volumes:

- The pixel's opacity gives the puff's extinction over the view chord through the sphere.
- The sun reaches the point under the pixel through the chord toward the sun, so the sun side keeps a
  bright rind and the core and far side go dark. Transmission blends single scattering toward the
  two-stream diffuse `1 / (1 + ¾ (1 − g) τ)` by the particle's albedo: snow powder carries light deep
  in, soot does not.
- The sky lights the top and the ground the bottom (the renderer's hemisphere ambient, mapped to
  display units like the sun). The old scalar ambient was a fraction of the sun's colour, which is why
  white snow dust under a low sun read tan.

`quality::puff_shade_volumetric` is the CPU mirror. Frames without an atmosphere (and the offscreen
harness) keep the old shading. The host sees `LayerCamera::sky_ambient`; `VfxGpu::set_sky_light` sets it
directly.

Snow dust is thinner than soil dust (`GroundClass::Snow` opacity 0.55) and carries a few darker puffs of
frozen earth (`lifted_earth`).

## Agent

- `render.grey_card { elevation_deg, aerosol_turbidity?, angstrom?, albedo?, sky_fraction? }` returns
  `engine` and `ies` (direct and diffuse lux, sunlit and shade luminance, `shade_ratio`), `sky_cd_m2`,
  `card_over_sky`, `engine_over_ies` and `metered`. Defaults: turbidity 0.18, Ångström 1.3, albedo 0.18,
  a quarter of the frame sky. A non-number, or `elevation_deg` outside (0, 90], answers `F0002`.
  A finite `aerosol_turbidity`, `angstrom`, `albedo` or `sky_fraction` outside its range is clamped
  (turbidity 0 to 4, Ångström 0 to 3, albedo and sky fraction 0 to 1).
- `terrain.get_ground_look` also returns `layer_albedo`: per layer `name`, `tint`, `texture_mean`,
  `effective` and `warning`.
- Guide topic `daylight`.

```json
{"method": "render.grey_card", "params": {"elevation_deg": 21.0}}
```
