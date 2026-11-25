# Lighting, shadows and global illumination

<!-- index: Directional light -->
<!-- index: Point light -->
<!-- index: Spot light -->
<!-- index: GiLook -->
<!-- index: DDGI -->
<!-- index: Baked GI -->
<!-- index: Lightmap -->
<!-- index: Path tracer -->
<!-- index: Screen-space GI -->
<!-- index: Reflection probe -->

Regia Engine lights scenes in physical units: the sun in lux, point lights in lumens, spot lights in
candela, the sky in candela per square meter, and the camera exposed in EV100. This chapter covers
the light types, shadows, indirect light (sky, ambient occlusion, irradiance probes, screen-space
GI), reflections and interiors. What the defaults look like, and the order to tune them in, is in
[The default look](../user-guide/default-look.md). The task order (sun, a GI look, an interior,
a bake) is [Light a level](../how-to/picture/light-a-level.md).

## Light components

| Component | Units | Position and direction |
|---|---|---|
| `DirectionalLight` (the sun) | `illuminance` in lux | `direction` field: the way the light travels. Up to four per frame: the lowest entity is the sun and owns the cascaded shadow map; the other three are unshadowed. |
| `PointLight` | `intensity` in lumens, `radius` cutoff in meters | the entity's `GlobalTransform` |
| `SpotLight` | `intensity` in candela, `inner_angle` / `outer_angle` half-angles in radians, `range` | position from the transform; aims along the entity's −Z axis (rotate the entity to aim) |
| `LightSourceSize` (optional, next to a `PointLight` or `SpotLight`) | `radius_m`: radius of the emitting sphere in meters (default 0.45) | sets the light's shadow softness and highlight size (see below) |
| `LightProfile` (optional, next to a `PointLight` or `SpotLight`) | `profile`: an IES profile's name in the `LightProfileLibrary`; `file_intensity`, `scale`, `rotation` | shapes the light with the IES distribution; nadir = a spot's direction or a point light's rotated −Y (see below) |
| `RectLight` | `intensity` in lumens per face, `width` / `height` in meters, `two_sided`, `range` | a rectangle on the entity's X (width) and Y (height), emitting toward its −Z |
| `TubeLight` | `intensity` in lumens, `length` and `radius` in meters, `range` | a cylinder along the entity's X axis |
| `LightSourceLtc` | a marker next to `LightSourceSize` | shades the sphere through the LTC tables with the path tracer's `1 / d²`: correct below the horizon, brighter near the light than the default form |

All three have `color` (linear RGB) and `cast_shadows`. Reference levels: a clear noon sun is
about 100 000 lux (50 000 at a low sun), an overcast day 10 000, a lit office 500. A 60 W-class
bulb is about 800 lumens.

**Light size.** A point or spot light without `LightSourceSize` has a 45 cm shadow source and a
pinpoint highlight. Add `LightSourceSize { radius_m }` to give it a physical size. A 2 cm LED
casts sharp shadows and a small, bright highlight on gloss. A 30 cm floodlight lens casts soft
shadows and a broad highlight. The shadow penumbra scales with the radius (up to the filter's
10-texel cap). The highlight on glossy surfaces widens and dims so that its total energy stays
the same. Diffuse light does not change. `radius_m: 0.0` is a true point. Agents edit the size
with `editor.set_property` (see [Light source size](../agents/light-source-size.md)).

**IES profiles and area lights.** Add a `LightProfile` to shape a point or spot light with a
measured IES LM-63 distribution (cooked from an `.ies` file by the asset pipeline, or added by
the `render.set_light_profile` agent method). `RectLight` and `TubeLight` are area lights shaded
like glowing panels and tubes. Their shadow is a soft, static distance probe traced from the
light, not a shadow map. Profiles shade on Desktop only; see
[IES light profiles and area lights](../agents/light-profiles.md).

```rust
use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{PointLight, SpotLight};
use regia_scene::Transform;

fn spawn_lamps(mut commands: Commands) {
    // A warm bulb, 800 lm, reaching 8 m.
    commands.spawn((
        Transform::from_translation(Vec3::new(0.0, 2.5, 0.0)),
        PointLight {
            position: [0.0; 3], // the runtime reads the transform
            color: [1.0, 0.85, 0.7],
            intensity: 800.0,
            radius: 8.0,
            cast_shadows: true,
        },
    ));
    // A floodlight pointing down at 45°, 20 000 cd.
    commands.spawn((
        Transform {
            translation: Vec3::new(4.0, 6.0, 0.0),
            rotation: Quat::from_rotation_x(-std::f32::consts::FRAC_PI_4),
            ..Transform::default()
        },
        SpotLight {
            position: [0.0; 3],
            direction: [0.0, 0.0, -1.0], // ignored as a component: the entity's −Z aims
            color: [1.0; 3],
            intensity: 20_000.0,
            outer_angle: 0.6,
            inner_angle: 0.45,
            range: 30.0,
            cast_shadows: true,
        },
    ));
}
```

**How many lights.** A view uploads up to `max_visible_lights` point and spot lights (Desktop
High 256, Ultra 512, Deck 96, Mobile Low 32; see [Render profiles](../reference/render-profiles.md)).
With more in view, the dimmest by screen importance are dropped and a warning is logged, never
silently. Lights are assigned to froxel clusters (forward+), at most `max_lights_per_cluster` per
cluster.

## Shadows

- **Sun**: cascaded shadow maps. Desktop High uses four cascades with PCSS-lite soft shadows: a
  blocker search, then a filter as wide as the gap between receiver and blocker times the sun's
  angular radius (0.27°, the real sun's), so contact shadows are crisp and long shadows soften.
  Cascade seams blend over the last 15 % of each cascade and the last one fades out. Every preset
  keeps each cascade on a fixed texel lattice: its center snaps to its own texel grid and its size
  steps on a ladder 2.2 % apart, so turning, moving or zooming the camera does not make static
  shadows shimmer (`FrameQuality::stable_sun_cascades`, ADR-1236; check with
  [`render.shadow_stability`](../agents/shadow-stability.md)). With `paged_sun_shadows`, each
  cascade is cached as pages that only redraw where a caster moved.
- **Local lights**: one shadow atlas (64 tiles). A shadowed spot takes one tile, a point light
  six (a cube), up to the profile's `local_shadow_max_lights`. When the casters do not all fit,
  the nearest win. Each light's penumbra follows its `LightSourceSize` (45 cm without one).
- **Translucent casters** (alpha-blended and transmissive materials: glass, canopies, visors)
  shadow by what they let through. They draw into a second, coarser map beside the opaque one
  (half the cascade edge; a quarter of the local atlas edge) holding the RGB transmittance product
  and the nearest translucent depth, and a receiver behind them takes that light: `1 − alpha` for a
  blended surface, `transmission × base color` for transmissive glass. Additive materials and
  meshes with `ShadowCast::Off` do not cast. Opaque shadowing is unchanged, and a frame without a
  translucent caster renders exactly as without the feature. The profile sets
  `translucent_shadow_cascades` (Mobile Low 0, Mobile High 1, Desktop Low and Deck 2, Desktop
  High and Ultra 4) and `translucent_local_shadows` (off on both Mobile tiers). The shadow report
  (`FrameRenderer::shadow_cache_report`) counts `translucent_casters`, `translucent_passes` and
  `translucent_draws`. A receiver between two translucent layers gets both layers' tint (one depth
  per texel); glass is not tinted by other glass; terrain and volumetric fog ignore translucent
  casters.
- **Bias** is applied on the receiver (normal offset plus a slope term), identically on every GPU.

The profile picks cascade count, resolution and filter (`csm_cascade_count`, `csm_resolution`,
`shadow_filter`). Shadows are the main lever on Mobile and Deck cost.

## Indirect light

`RenderSettings.gi_look` picks the player-facing indirect light. `None` keeps the profile default.

| `GiLook` | Light | Where it runs |
|---|---|---|
| `Balanced` | Sky irradiance, a sunlit-wall bounce, a mesh ground bounce. An installed baked volume is ignored. | Every platform. Medium forces the profile's probe column off and lands here unless the player chose another look. |
| `Depth` | GPU irradiance probes (DDGI). | Desktop. `GiLook::depth_available` is false on Android. |
| `Baked` | The map's `.fgiv` volume. With no volume installed, the frame shades as Balanced. | Any tier. A map that names a volume also uses it under Depth and under the preset default, including Low, with DDGI and screen-space GI off. |

**Sky and ambient.** With `Environment::ibl` on (the default), the procedural sky lights
everything that sees it: blue from above, a neutral ground bounce from below, plus reflections.
With it off, `Environment::ambient_lux` is a flat fill.

**Ambient occlusion.** The profile's `ssao` mode darkens creases and contacts (half resolution
on Desktop High, 1.2 m radius, multi-bounce) and also occludes sky reflections in crevices.

**Irradiance probes** (`GiMode::Probes`, Desktop High and Ultra). A `GiField` is a grid of probes
that store the light arriving from every direction: open sky, sunlit ground bouncing into
shadow, the darkness under a roof. Inside the grid the probes *replace* the sky term, fading
back to it one cell outside outdoors. Indoors, up-facing surfaces (`n.y` near 1) keep full probe
confidence for one extra cell and return to the sky at two cells. You fill the field by tracing it against a signed-distance
description of the scene (`SdfScene`: boxes, spheres, voxelized meshes, the terrain
heightfield), on the CPU, and hand it to the renderer:

```rust
use regia::prelude::*;
use regia_render::{GiField, ProbeSun, RuntimeRenderer, SdfScene, SdfShape, update_irradiance};

/// Builds an 8×3×8 probe grid over a courtyard and installs it.
fn bake_courtyard_gi(mut renderer: ResMut<RuntimeRenderer>) {
    let Ok(mut field) = GiField::new([-14.0, 0.5, -14.0], [4.0, 2.0, 4.0], [8, 3, 8]) else {
        return;
    };
    let scene = SdfScene {
        shapes: vec![
            // Ground.
            SdfShape::Box { center: [0.0, -0.5, 0.0], half: [30.0, 0.5, 30.0], albedo: [0.35; 3], emissive: [0.0; 3] },
            // A red wall that should tint its surroundings.
            SdfShape::Box { center: [0.0, 2.0, -6.0], half: [6.0, 2.0, 0.3], albedo: [0.6, 0.1, 0.08], emissive: [0.0; 3] },
        ],
    };
    let sun = ProbeSun { direction: [0.3, -1.0, -0.4], color: [1.0, 0.98, 0.95], illuminance: 100_000.0 };
    // Each update adds a bounce; a few updates converge.
    for _ in 0..4 {
        update_irradiance(&mut field, &scene, sun, 64, 0.5);
    }
    renderer.set_gi_field(Some(field));
}
```

A grid holds at most `MAX_PROBES` (256) probes: cover the places the camera spends time (a base,
a courtyard, a cockpit) rather than the whole map. `update_irradiance_with_lights` adds local
lights to the trace and can treat misses as black for closed interiors;
`update_irradiance_with_sky` takes the radiance an escaping ray sees from the atmosphere tables,
for daylit interiors whose openings let the real sky in. `rays` is clamped to 8..=128.
`hysteresis` is clamped to 0.0..=0.99, so an update every frame near 1 keeps 99 % of the
previous spherical harmonics and takes 1 % of the new trace. Depth moments are replaced
every update, and a probe whose centre is inside a solid is cleared.

**Screen-space GI** (`screen_space_gi`). A half-resolution gather of light from nearby on-screen
surfaces, the cheaper tier. A held camera reuses the previous result.

**Planet-shine.** A level's sky body (Jupiter seen from Europa, for example) lights the ground as
a shadowless directional fill; see [Rendering](../user-guide/rendering.md).

## Reflections

Screen-space reflections march the depth buffer for floors and wet surfaces, scaled by roughness
and fall back to
reflection probes on a miss. A probe is captured by the renderer
(`FrameRenderer::capture_reflection_probe`) and installed with
`RuntimeRenderer::set_reflection_probes`; the renderer picks one list of probes by distance from the
camera (hard-capped at 4 uploaded), then weights them per pixel with box projection.

Two SSR paths exist. The stock Desktop High and Ultra presets run stochastic SSR: roughness
cutoff 0.55 with a fade starting 0.15 below it, and a miss takes a probe whenever the reflection
ray points below the horizon (no floor-only test). The linear march (lab, editor, probe capture)
scales by roughness (a 0.35-rough floor stays a mirror, zero at 0.7) and gives a probe only to
floors with `normal.y` at least 0.72.

## Interiors

A cockpit or a tunnel lit only by local lights reads as black at an outdoor exposure. Two pieces
fix that:

1. An `InteriorVolume { center, half_extent, ambient_lux }` around the room. Surfaces inside it
   drop the sky term and take a neutral 40 lux floor (or `ambient_lux`) plus probe bounce; the
   sunlit exterior seen through a door keeps the sky. Fit the box to the room's inner faces: a wall,
   floor or roof that lies on a face (or within 2 cm outside it) counts as inside, so it takes the
   interior shade on every pixel. Auto exposure lifts a fully indoor view; for
   a fixed exposure, EV 8 is the interior reference. Without a volume, an `EnclosureScene` (a hollow SDF shell)
   lets the renderer detect enclosure itself.
2. `build_interior_lighting` places a probe grid that covers the room and traces one bounce of the
   local lights (misses stay black, so the mouth of a tunnel is not painted with sky). Bind it
   with `set_gi_field` and capture one reflection probe at its center.

The [agent guide's high-end lighting page](../agents/high-end-lighting.md) has the exact numbers
and the checks.

## Seeing what the lighting does

- Turn features off one at a time in `RenderSettings::profile` (`gi`, `ssr`, `ssao`,
  `screen_space_gi`) and compare screenshots; the agent protocol's `seeing.compare_images` gives
  a numeric difference.
- `RuntimeRenderer`'s frame renderer reports per-pass GPU times (`gpu_timings()`), including
  `ssgi_ms` and `ssr_ms`; probe tracing is CPU time (`gi_cpu_ms`).
- `render_lab` and `samples/lookdev` render reference scenes and print timings
  ([Look-dev harness](../user-guide/lookdev.md)).

A ground-truth path tracer (`regia_pt`) renders the same scenes for comparison; renderer
changes are measured against it (`docs/specs/27-renderer-excellence.md`). The tracer is offline.
It does not ship in a game build, and runtime crates do not depend on it. It also writes the
baked volumes (`regia bake-gi`) and the lightmaps (`regia bake-lightmaps`). A lightmap
(`.flmap`, `look.lightmap`) stores irradiance per texel for static meshes and replaces diffuse
indirect light on `LightmapReceiver` entities. The profile caps that lookup: bilinear on Mobile
Low, bilinear plus directions on Mobile High, and the full bicubic-and-directions settings on
the other presets (`RenderProfile::lightmap_tier`). Derived LOW and ULTRA_LOW lightmaps still
miss their original seam and dark-point checks. The agent page is
[Lightmaps](../agents/lightmaps.md). The editor's one-box probe bake is
[The Bake panel](../user-guide/bake-panel.md).
