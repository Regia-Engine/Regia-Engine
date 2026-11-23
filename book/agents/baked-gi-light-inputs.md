# Bake light inputs: HDRI skies and area emitters

A GI bake (`regia bake-gi map.bake.ron`, [Baking GI from meshes](baked-gi-bake.md)) is lit by the sun, the
sky, sky bodies and emissive surfaces. `.bake.ron` v2 (ADR-1095) adds two inputs:

- **`sky_hdri`**: an equirect HDRI map as the sky, with an optional **sun extraction** that lifts the sun out
  of the map into the bake's sun.
- **`sky_atmosphere`**: the runtime's physical sky (`planet: Earth`, `Mars` or `Airless`) for the description's
  sun direction, the same sky-view table the raster frame draws and lights with.
- **`emitters`**: area emitters (quads, discs, spheres) of a given radiance or luminous power, without a mesh
  file.

`--dry-run` reports what lights the bake under `plan.lights`, so an agent can check the energy of the inputs
before it spends a bake on them. The format is specified in `docs/formats/bake-ron.md`.

## Describing the lights

```ron
(
    format_version: 2,
    // A clear-day HDRI, turned 30° about +Y, scaled so an upward surface gets 80 000 lux from it (sun included).
    sky_hdri: Some((source: "sky/clear_noon.hdr", rotation: 0.5236, irradiance: Some(80000.0), extract_sun: true)),
    emitters: [
        // A 1500 lm ceiling panel facing down.
        (id: Some("hangar_panel"),
         shape: Quad(corner: (-1.0, 6.0, -0.5), edge_u: (2.0, 0.0, 0.0), edge_v: (0.0, 0.0, 1.0)),
         power: Some(1500.0), color: (1.0, 0.9, 0.8)),
        // A round beacon lens, 0.2 m, both faces, 2000 cd/m².
        (shape: Disc(center: (12.0, 3.0, -40.0), normal: (1.0, 0.0, 0.0), radius: 0.2), radiance: Some((2000.0, 300.0, 100.0)),
         two_sided: true),
        // A glowing sphere, 0.15 m.
        (shape: Sphere(center: (4.0, 1.0, 2.0), radius: 0.15), radiance: Some((500.0, 500.0, 450.0))),
    ],
    layout: (bricks: [(min: (-4.0, 0.0, -4.0), max: (4.0, 6.0, 4.0), spacing: 1.0)]),
)
```

- **Mapping.** Row 0 of the map is the zenith (+Y). The image's horizontal centre looks along −Z, and `u`
  grows toward +X. `rotation` turns the map about +Y (radians; positive turns +Z toward +X).
- **Units.** Map values × `scale` are cd/m². HDRIs are usually relative, so give `irradiance` (the lux an
  upward surface should get from the map and the extracted sun), or a `scale`.
- **Size.** Maps wider than `max_width` (2048) are box-filtered down by an integer factor, keeping their light.
- **Sun extraction.** The probes see the sky through a fixed ray set, which rarely hits a sun-sized hot spot,
  and the runtime keeps the direct sun live anyway. With `extract_sun`, the brightest region above
  `sun_threshold` × the map's mean luminance becomes a sun disc carrying its excess light. Its texels take the
  mean of the ring around it. The sun's bounce is baked (sun component); the sun seen directly is not. A region
  larger than `sun_max_solid_angle` (0.03 sr), or whose directions are not a compact disc (a full-width latitude
  band has a disc's area, but the vectors cancel), is refused (`bake_hdri_sun`) and the map is left unchanged.
  It is not a sun: raise the threshold or turn extraction off. Do not give a `suns` entry as well.
- **Atmosphere.** `sky_atmosphere: Some((planet: Earth, height: 2.0, scale: None))` needs a sun (the table
  follows the first one's direction) and excludes `sky` and `sky_hdri`. The table's level is the planet's own sun at the top of
  the atmosphere; `scale` rescales it. `plan.lights.sky.source` is `atmosphere:earth`.
- **Emitters** are black emissive triangles: probes see them directly and through bounces, and their light
  lands in the emissive component. They reflect nothing, and they occlude. A `power` emitter's radiance is
  `color · power / (π · area · sides)`, with the area of the traced triangles, so the power is exact.

## More than one sun (`.bake.ron` v3)

Since format v3 (ADR-7818) a description lists its suns, up to four:

```ron
(
    format_version: 3,
    suns: [
        (toward: (0.3, 0.5, 0.2), illuminance: 50000.0, angular_radius: 0.00465),
        (toward: (-0.6, 0.3, -0.4), illuminance: 8000.0, color: (1.0, 0.75, 0.55)),
    ],
    sky: Some((2500.0, 3500.0, 5000.0)),
    layout: (bricks: [(min: (-4.0, 0.0, -4.0), max: (4.0, 6.0, 4.0), spacing: 1.0)]),
)
```

- The first sun is the scene's sun: `sky_atmosphere` follows it, and the volume records it as the baked sun.
  The others are the scene's extra suns (`PtScene::extra_suns`), the same lights the frame's extra
  `DirectionalLight`s become ([Render lights](render-lights.md)).
- Every bake takes every sun: the GPU and CPU probe bakes put each sun's bounce in the sun component, and the
  CPU and GPU lightmap bakes also bake each one's direct light with `direct_sun`. The energies add: a bake of
  both suns is the bake of each alone, summed (`crates/regia_pt/tests/multi_sun_bake.rs`).
- A fifth sun is refused (`bake_desc_invalid` at `suns`). A v3 file with the old `sun` key, or a v0-v2 file
  with `suns`, is refused with `bake_desc_invalid` naming the key.
- Older files keep loading: their one `sun` reads as a one-element `suns`, and `regia upgrade-project --write`
  rewrites `sun: Some(s)` to `suns: [s]` on disk. A one-sun file bakes the same bytes before and after.
- With `follow_sun`, the runtime scales the whole sun component by the frame's sun over the first baked sun,
  so the other suns' bounce follows the first sun's level. Bake several suns for a scene that keeps them
  at the baked levels.
- The level export (`level.export_bake`, `regia bake-gi --from-scene`) and the editor's bake panel still write
  one sun (`render.lights` lists them under `sun_only_paths`).

## Checking the inputs: `plan.lights`

```text
"plan": { .., "lights": {
  "sun": { "toward": [-0.41, 0.72, -0.56], "illuminance": 71200.0, "color": [1.0, 0.97, 0.92], "angular_radius": 0.0093, "from_hdri": true },
  "sky": { "kind": "equirect", "source": "sky/clear_noon.hdr", "source_size": [8192, 4096], "size": [2048, 1024],
           "downsample": 4, "scale": 0.81, "rotation": 0.5236, "upward_irradiance": [9100.0, 10900.0, 14100.0],
           "mean_luminance": 2100.0,
           "sun_extraction": { "toward": [..], "color": [..], "illuminance": 87900.0, "angular_radius": 0.0093,
                               "solid_angle": 0.00027, "texels": 61, "threshold": 213000.0 } },
  "sky_bodies": 0,
  "extra_suns": [ { "toward": [-0.6, 0.3, -0.4], "illuminance": 8000.0, "color": [1.0, 0.75, 0.55], "angular_radius": 0.0, "from_hdri": false } ],
  "emitters": [ { "id": "hangar_panel", "shape": "quad", "area": 2.0, "radiance": [254.0, 228.0, 203.0],
                  "power_lm": 1500.0, "triangles": 2, "two_sided": false }, .. ],
  "emitter_power_lm": 2240.0, "emissive_materials": 3 } }
```

The numbers are illustrative. What to check:

- `sky.upward_irradiance` (lux per channel, the sky alone) plus `sun.illuminance × sun.toward[1]` is what an
  open upward surface receives. A clear day is tens of thousands of lux. Single digits mean a missing `scale`.
- `sky.sun_extraction` is `null` with `extract_sun: true` when no texel passed the cut (an overcast map: fine).
- `emitters[].power_lm` matches the fixture you meant (a 60 W-class bulb is about 800 lm).
- `extra_suns` lists the suns after the first (v3). It is absent with one sun or none.
- `sky.kind` is `equirect` for an HDRI, `uniform` for `sky`, `table` for an atmosphere table, `none` for airless.

## Energy checks

`regia_pt::analytic` holds closed-form references, and `regia_pt::env_map` holds the CPU side of the sky
lookup. The tests in `crates/regia_pt/tests/bake_light_inputs.rs` hold the probe tracer to them. For an
emitter of uniform radiance `L` symmetric about the axis `a` from the probe:

| Quantity | Reference |
|---|---|
| `L00` | `Y00 · L · Ω` (`Ω` = solid angle) |
| `L1` | `Y1 · L · Ω⊥ · a` (`Ω⊥ = ∫ cos θ dω`, π × the form factor) |
| Disc, radius `r`, distance `h` on axis | `Ω = 2π(1 − h/√(h² + r²))`, `Ω⊥ = π r²/(r² + h²)` |
| `2a × 2b` rectangle, distance `h` on axis | `Ω = 4 asin(ab/√((a² + h²)(b² + h²)))`, `Ω⊥` from the corner form factor × 4 |
| Sphere, radius `r`, distance `d` | `Ω = 2π(1 − √(1 − r²/d²))`, `Ω⊥ = π r²/d²` |
| Uniform sky / upper hemisphere | `Ω = 4π`, `Ω⊥ = 0` / `Ω = 2π`, `Ω⊥ = π` |
| Lambertian ground of albedo ρ under irradiance E | radiance `ρ E / π` over the lower hemisphere |
| Any HDRI or atmosphere table | `env_map::sky_sh(sky, rows)`: the quadrature of the same bilinear lookup the kernels make |

Covered on the GPU: a constant HDRI equals the uniform sky, an upper-hemisphere HDRI matches the analytic moments,
a turned smooth HDRI and an Earth atmosphere table from a description match their CPU projections, the HDRI's first
moment points along the turned lobe, HDRI sky and
an extracted sun bounced off a ground land in the sky and sun components with ρE/π, a one-sided disc emits
nothing backward, and the disc, quad (in lumens) and sphere match their formulas. On the CPU: the formulas
against numerical quadrature, sun extraction conserving the light on surfaces facing many ways, RGBE decoding,
and `power` setting the radiance from the traced area.

## Errors

| Code | Meaning |
|---|---|
| `bake_desc_invalid` | `sky` with `sky_hdri`; `sky_atmosphere` with another sky or without a `sun`, or a non-positive `height`; `extract_sun` with a `sun`; `scale` with `irradiance`; a non-finite `rotation`; `max_width` below 16; `sun_threshold` not above 1; an emitter without exactly one of `radiance` and `power`, with a black `color`, a degenerate quad, a zero radius or normal, or `segments` out of range. `path` names the key (`sky_hdri.irradiance`, `emitters[2].shape.radius`). |
| `bake_source_missing` | The HDRI file does not exist (`sky_hdri.source`). |
| `bake_hdri_decode` | The HDRI does not decode: not RGBE or uncompressed EXR, an unsupported orientation or format line, truncated pixels, negative or non-finite texels. |
| `bake_hdri_sun` | Sun extraction found a region above the cut larger than `sun_max_solid_angle`, or a region that is not a compact disc (a full-width latitude band). The map is unchanged. |
| `bake_desc_parse` | An unknown emitter shape (`Disk(..)`) or field. |

## API

- `regia_pt::bake_inputs`: `SkyHdriDesc`, `SkyAtmosphereDesc`, `PlanetPreset`, `EmitterDesc`, `EmitterShape`,
  `LightInputs::of_scene`, `load_sky_hdri`, `atmosphere_sky`, `add_emitter`.
- `regia_pt::env_map`: `EnvMap` (load, downsample, `extract_sun`, `into_sky`), `equirect_uv`, `sky_sh`,
  `sky_irradiance`, `PtSky::radiance`.
- `regia_pt::PtSky::Equirect`: an HDRI sky for any trace (camera renders too).
- `regia_pt::hdr`: the RGBE reader.
