# Volumetric fog

Desktop High and Ultra march a froxel grid (Wronski, SIGGRAPH 2014). Each cell stores
in-scatter and extinction. The sun is multiplied by cascade 0 of the shadow map (the paged
sun atlas when paging is on, otherwise the CSM slice), and up to
four local lights add inverse-square light. Slices are integrated front to back and blended
with the previous frame after reprojection.

This is separate from `RenderSettings.fog`, the analytic height fog. That fog runs in the
forward shader only when the LUT sky is off and the world has air. On Earth the LUT sky is off
for Desktop Low, Deck, Mobile and the lab. Desktop High and Ultra draw the LUT sky on an Earth
frame with no studio environment, and that frame forces `FogSettings` off for meshes and
terrain. An airless world forces it off on every preset. The haze on a LUT-sky frame is the
atmosphere LUT and this froxel pass, not `FogSettings`.

## Controls

`VolumetricFogSettings` is the height component (density, falloff, albedo, anisotropy,
history weight). `FogVolumes` adds boxes and spheres. Both are resources. Density zero and
an empty volume list skip the pass.

```rust
use regia::prelude::*;
use regia::render::{FogVolume, FogVolumes, VolumetricFogSettings};

fn configure(app: &mut App) {
    app.insert_resource(VolumetricFogSettings {
        density: 0.02,
        ..VolumetricFogSettings::default()
    });
    app.insert_resource(FogVolumes {
        volumes: vec![FogVolume::Sphere {
            center: [0.0, 2.0, 0.0],
            radius: 6.0,
            density: 0.2,
        }],
    });
}
```

Fog that belongs to a tunnel or a cave goes in its `InteriorVolume`:
`FogVolume::interior(volume, density)` is a box with the interior's centre and half extents, so the
fog exists exactly where the interior contains the point (ADR-3257). An interior that contains
nothing (a non-positive or non-finite axis) or a negative density adds no fog. The sky does not
light air inside an interior volume, so the interior's fog glows only where the sun or a fog light
reaches it: under a tunnel portal that is the sunlit wedge below the cut; deeper in, the headlight.
The tunnel acceptance scene (`crates/regia_terrain/tests/tunnel_scene`) checks both by readback:
`interior_fog_stays_in_the_tube` (no pixel outside the interior's screen box moves) and
`light_shafts_fall_through_the_cut` (with the `calibrated` sun-shadow rule, air the roof shadows
does not glow like the shaft).

Point lights extracted from the world are converted from lumens to candela (`lm / 4π`) before
the fog march, matching the forward shader. Spot lights are already candela and pass through
as is (before v0.2.0 they were divided by 4π too, 12.6× too dim). Paged shadows are not in
this tree; the shaft uses the cascaded shadow map.

## Surfaces, low sun and planets (look-r2)

- A froxel is sampled no deeper than the farthest surface its sub-rays hit, and a pixel reads
  the fog up to its own depth: height fog below the ground no longer leaks onto terrain seen
  downhill (the old Mars "fog wall").
- Sunlight and skylight in the fog are dimmed by the fog layer they cross
  (`fog_layer_optical_depth`): a dusk sun barely lights a valley haze.
- `planet_medium` (default on): on Mars the fog scatters like the planet's dust (butterscotch,
  forward lobe), not like white water fog. Earth keeps `albedo` / `anisotropy`.
- Fog inside an `InteriorVolume` sees no sky. A map can author its dust with
  `LevelAtmosphere.haze` instead of a `VolumetricFogSettings` resource.

## Local lights in the fog (v0.2.0, ADR-1365)

The fog has four local-light slots (`FOG_LIGHT_SLOTS`). Each frame, `extract_frame` scores
every point and spot light from the primary camera (the window view drawn first) and keeps
the four best:

```text
score = candela × max(color) × window(gap) / max(d², r²)
gap   = max(d − r, 0)            distance from the eye to the light's sphere
window(x) = (1 − (x / far)⁴)₊    far = the view's far plane (the froxel range)
```

`d` is the eye-to-light distance and `r` the light's radius (a spot's range). Inside its
radius every position scores as if it were at `r`. A light outside its own radius still
scores, so a muzzle flash 20 m ahead keeps its slot. A light picked the frame before ranks
with its score × 1.25 (`FOG_PICK_HYSTERESIS`), so two lights of about equal strength do not
swap every frame. Lights that stay keep their slot, and a new light takes a freed slot.

A spot lights only the air inside its cone. The ramp is the forward pass's: full inside the
inner half-angle, zero past the outer one, linear between. The distance falloff is the forward
pass's too, `saturate(1 − (d / r)⁴)² / (d² + 1)` (`fog_light_falloff`). A lamp at the eye, such
as a headlight, does not flood the slices next to the camera.

On Desktop High and Ultra a spot that casts shadows (`SpotLight.cast_shadows`) and has a tile
in the local shadow atlas is also shadowed in the fog: a floodlight through a doorway throws a
shaft, and the wall beside the door stops the beam. The fog takes one unfiltered atlas tap per
sample. Point lights are not shadowed in the fog.

Hosts that fill `OutdoorParams::lights` themselves (lab scenes, `FrameRenderer` without the
ECS) build each light with `FogLight::point(position, color, candela, radius)`,
`FogLight::spot(position, direction, color, candela, range, inner, outer)` or
`FogLight::from_local(&lab_local_light)`. `from_local` applies the same units as the
forward pass. Struct literals no longer compile: `FogLight` gained `cone`.

### Agent surface

`render.fog_lights` returns the pick for the last extracted frame:

```json
{ "slots": 4, "hysteresis": 1.25, "candidates": 6, "eye": [0, 1.8, 0],
  "lights": [ { "entity": 4294967301, "kind": "spot", "position": [0, 2, 0],
                "color": [1, 0.95, 0.85], "intensity_cd": 40000, "radius": 70,
                "score": 8.16, "held": true, "shadowed": false,
                "direction": [0, 0, -1], "cos_inner": 0.989, "cos_outer": 0.900 } ] }
```

`lights` is in slot order. `held` is true when the light was also picked the frame before.
`shadowed` is true when the last drawn frame shadowed the light from its atlas tile (false
without a `RuntimeRenderer`).
`direction`, `cos_inner` and `cos_outer` are present only for spots. The method fails with
the not-found error (`F0003`) when the app has no `FogLightPick` resource (no `RenderPlugin`).

Use it when a light is missing from the haze. A fifth light is not picked: its score is
lower than the four in `lights`. A light whose sphere lies past the far plane scores 0.

### What a good frame looks like

A spot aimed across the frame lights a wedge of haze on its side and leaves the other side
dark. A point light of the same candela lights both sides evenly
(`regia_render/tests/fog_local_lights.rs`). With a wall over the upper half of a shadowed
spot's cone, the haze above the axis goes dark and the haze below it stays lit. The per-sample in-scatter of a spot on its
axis equals that of a point light of the same candela (`fog_lights` unit tests).

## Sun shadow in the fog (ADR-3122)

The froxel fog reads one sun cascade: the first whose split reaches 150 m (cascade 0 is too
short for light shafts on a long view), with the same light matrix the shadow pass just drew.
`VolumetricFogSettings::sun_shadow` chooses how it reads it:

| Rule | Depth bias of the compare | Outer 15 % of the cascade's square | A froxel slice that holds a surface |
|---|---|---|---|
| `legacy` (default) | 0.002 of the cascade's depth extent: about 0.4 m along the sun on a 200 m view, metres on a long one | a shadowed sample fades to lit toward the edge | samples beyond the surface are pulled onto it |
| `calibrated` | half a texel plus half a texel per unit of level-ground slope seen from the sun (`fog_sun_shadow_bias_m`; ADR-4199 reduces the constant margin) | the compare decides up to the edge | every sub-ray's samples are spread over the air in front of its own surface (`fog_sample_depth`), weighted by the share of the slice that air fills (`fog_sample_weight`) |

Outside the square both rules read the sun as unshadowed: the fog has no other cascade, and past
the shadow distance the forward pass reads lit too. Under `legacy` the air within the bias under
a roof, the samples on a roof's underside and the shadowed air near the square's edge read the
sun; in the spec 27 fog room this lights the room's upper air and makes the raster fog 1.43
times as bright as the path tracer's. `calibrated` keeps that air dark. It changes every fogged
frame with sun shadows, so it is opt-in until the owner decides the default (the default frames,
`studio_frame_pin` and the shipped goldens, are `legacy`).

```rust
use regia_render::{FogSunShadowRule, VolumetricFogSettings};

fn calibrated_fog() -> VolumetricFogSettings {
    VolumetricFogSettings {
        density: 0.02,
        sun_shadow: FogSunShadowRule::Calibrated,
        ..VolumetricFogSettings::default()
    }
}
```

### Agent surface: `render.fog_shadow`

`render.fog_shadow { point? }` reports how the last drawn fog pass read the sun shadow (the
render plugin's `FogSunShadowProbe`, filled by every fog pass). The shape (the values are
illustrative):

```json
{ "recorded": true, "rule": "calibrated", "configured_rule": "calibrated",
  "rules": ["legacy", "calibrated"], "fade_band": 0.0,
  "cascade_index": 3, "cascade_count": 4, "sun_travel": [0.5, -0.866, 0.0],
  "bias_ndc": 0.000186, "bias_m": 0.126,
  "cascade": { "resolution": 2048, "texel_m": 0.16, "depth_m": 680.0 },
  "lookup": { "point": [-4.0, 5.7, 0.0], "reads": "compare", "why": "covered",
              "state": "covered", "edge": 0.03, "fade": 0.0, "texel": [1031, 1010],
              "ndc_z": 0.297, "bias_ndc": 0.000186,
              "cascades": [ { "index": 0, "read": false, "state": "not_read" },
                            { "index": 1, "read": false, "state": "not_read" },
                            { "index": 2, "read": false, "state": "not_read" },
                            { "index": 3, "read": true, "state": "covered" } ] } }
```

- `recorded` is false until a fog pass has run; the reply then lists `rule` (the configured
  one), `rules` and `fade_band` only.
- `rule` is the rule the last pass ran, `configured_rule` the `VolumetricFogSettings` resource's.
- `cascade` is null when the fog ran without sun shadows (every sample reads the sun).
- `bias_m` is the compare's bias in metres along the sun: air nearer than that under a caster
  reads lit.
- `lookup` (with `point`) runs the shader's lookup on the CPU at that world point. `state` is
  `covered` (the compare decides), `faded` (`legacy` only: in the fade band, a shadowed sample
  still reads `fade` of the sun) or `outside` (read as lit). `reads` is `compare`,
  `compare_faded` or `lit`, and `why` says why: `covered`, `fade_band`, `outside_cascade` or
  `no_sun_shadow`. Per cascade, the one the fog read carries the state; the others are
  `not_read`. The stored depth is not read back: the compare's result needs the frame.

Errors: `F0003` (not found) when the app has no `FogSunShadowProbe` resource (no
`RenderPlugin`); `F0002` (invalid params) when `point` is not three finite numbers.

Use it when fog glows where the sun should be blocked: a `faded` or `outside` state, or a
`bias_m` larger than the gap between the caster and the air, explains the light; `calibrated`
fixes the first and the last.

### What a good frame looks like (sun shadow)

In a closed room with a slit in its roof, the shaft from the slit to the floor is the only lit
air: pixels whose rays never come near the shaft stay dark
(`regia_render/tests/fog_shadow_lookup.rs`, the room on lavapipe under `calibrated`). The CPU
tests check the lookup against an analytic shadow map: air under a roof slab deeper than the
cascade resolves reads no sun at five cascade sizes and five places in the square, air above it
keeps the sun, and lit level ground does not shadow itself.
