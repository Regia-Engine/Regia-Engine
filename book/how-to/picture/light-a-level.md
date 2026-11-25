# Light a level

<!-- index: Directional light -->
<!-- index: Point light -->
<!-- index: Spot light -->
<!-- index: GiLook -->
<!-- index: Balanced -->
<!-- index: DDGI -->
<!-- index: Baked GI -->
<!-- index: Lightmap -->
<!-- index: Path tracer -->
<!-- index: Interior volume -->
<!-- index: Post volume -->
<!-- index: Exposure -->

Place a sun, local lights, and one indirect-light path, then check the image at the exposure the
camera will use. Units, shadow columns, and the probe API are in
[Lighting, shadows and GI](../../systems/lighting.md). The bake UI is
[The Bake panel](../../user-guide/bake-panel.md). Place-specific grade is
[Post volumes](../../user-guide/post-volumes.md).

## Before you start

The default look is a clear-day sun near 100 000 lux, a procedural sky, histogram auto exposure,
and AgX. Desktop High and Ultra add irradiance probes. Medium, Low, VeryLow, Deck, and the mobile
tiers start on the analytic sky and ambient terms (`GiLook::Balanced`) unless a baked volume is
installed. Set the quality tier first
([Quality and upscalers](quality-and-upscalers.md)): a light that looks right at High can read as
softer at VeryLow, which draws Desktop Low at one third of the output in each axis (about a
fifth of Low's pixels) and upscales with SGSR 1.

## 1. Add the sun and the local lights

A frame draws up to four `DirectionalLight` components. The lowest entity is the sun and owns the
cascaded shadow map. The other three are unshadowed. Point and spot lights are uploaded up to the
profile's `max_visible_lights` (Desktop High 256, Ultra 512, Deck 96, Mobile Low 32). A larger
set is ranked and the drop is logged.

| Component | Unit | Aim |
|---|---|---|
| `DirectionalLight` | lux | `direction` is the way the light travels |
| `PointLight` | lumens | the entity's transform |
| `SpotLight` | candela | the entity's −Z |
| `RectLight` | lumens per face | a rectangle on the entity's X and Y, emitting along −Z |
| `TubeLight` | lumens | a cylinder along the entity's X |

Reference levels: noon sun about 100 000 lux, overcast 10 000, a lit office 500, a 60 W-class
bulb about 800 lumens. Add `LightSourceSize { radius_m }` when the highlight and the shadow
penumbra should match a real source. `radius_m: 0.0` is a point. An IES `LightProfile` shapes a
point or spot on desktop presets.

<!-- ignore-reason: a spawn sketch of light field names; not a system registered on an app -->
```rust,ignore
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_render::{DirectionalLight, PointLight};
use regia_scene::Transform;

fn sun_and_bulb(mut commands: Commands) {
    commands.spawn(DirectionalLight {
        direction: [0.3, -1.0, -0.4],
        color: [1.0, 0.98, 0.95],
        illuminance: 100_000.0,
        cast_shadows: true,
        ..DirectionalLight::default()
    });
    commands.spawn((
        Transform::from_translation(Vec3::new(0.0, 2.5, 0.0)),
        PointLight {
            position: [0.0; 3],
            color: [1.0, 0.85, 0.7],
            intensity: 800.0,
            radius: 8.0,
            cast_shadows: true,
        },
    ));
}
```

`direction` is the way the light travels, as three floats, not a normalized glam vector the
component stores for you. `PointLight.position` is unused on a spawned entity: extraction reads
the transform. `render.lights` lists which paths take the extra directional lights
(`extra_directional_paths`) and which stay sun-only (`sun_only_paths`).

## 2. Choose the indirect light

`RenderSettings.gi_look` is the player-facing choice. `None` keeps the profile's default.

| `GiLook` | What the frame uses |
|---|---|
| `Balanced` | Sky irradiance, a sunlit-wall bounce, and a mesh ground bounce. Every platform. An installed baked volume is ignored. |
| `Depth` | GPU irradiance probes (DDGI). Desktop builds. On Android the look is not available (`GiLook::depth_available` is false). |
| `Baked` | The map's `.fgiv` volume. Without a volume the frame shades as Balanced. |

A map that has a volume also uses it under `Depth` and under the preset default, including Low,
with DDGI and screen-space GI off.

<!-- ignore-reason: one field write on RenderSettings; not a full frame setup -->
```rust,ignore
use regia_render::{GiLook, RenderSettings};

fn baked(settings: &mut RenderSettings) {
    settings.gi_look = Some(GiLook::Baked);
}
```

**Baked GI volume (`.fgiv`).** In the editor, open the Bake tab, fit a box to the static set,
pick Draft, Normal, or High, and press Bake. Mark the meshes that should occlude with
`GiMobility::Static`. Dynamic meshes are skipped. The panel writes
`bake/<level>.editor.bake.ron` (not shipped) and `assets/maps/<level>.fgiv`. Name that file in
the level's `look.baked_gi`, or tick **Name the volume in the level when done**. The same bake
from a shell is `regia bake-gi`. A volume is one box. A whole level, interiors, or terrain use a
hand-written `.bake.ron` (automatic bricks). The probe count refuses a non-positive size, an axis
over 100 000 m, or more than 4 194 304 probes.

**Lightmaps (`.flmap`).** A lightmap stores baked irradiance per texel for static meshes, in a
second UV set. `regia bake-lightmaps --scene assets/levels/yard.level.ron --write-level` writes
`assets/maps/yard.flmap` and sets `look.lightmap`. Opening the level installs it and attaches a
`LightmapReceiver` to each baked part. Receivers take diffuse indirect light from the lightmap.
Dynamic meshes keep the probe or ambient term. The profile caps the lookup
(`RenderProfile::lightmap_tier`): bilinear on Mobile Low, bilinear plus directions on Mobile
High, and the full settings (bicubic and directions) on the other presets. Derived LOW and
ULTRA_LOW lightmaps still miss their original seam and dark-point checks; the Mobile Low room
passes its numerical bars, and the Mobile Low reference image is waiting on an owner re-bless.
Details and the agent methods are on [Lightmaps](../../agents/lightmaps.md).

**Screen-space GI and reflections.** Desktop High and Ultra also gather a half-resolution
screen-space bounce and run stochastic screen-space reflections, falling back to reflection
probes. Capture a probe with `FrameRenderer::capture_reflection_probe` and install it with
`RuntimeRenderer::set_reflection_probes`. At most four probes are uploaded, chosen by distance.

## 3. Light an interior

An indoor set at an outdoor exposure tonemaps toward black. Two pieces fix that:

1. An `InteriorVolume` fitted to the room's inner faces. Surfaces inside drop the sky term and
   take a neutral 40 lux floor plus probe bounce. Auto exposure lifts a fully indoor view. For a
   fixed exposure, EV 8 is the interior reference.
2. A probe grid from `build_interior_lighting` (one bounce of the local lights; a miss stays
   black) and one reflection probe at the room's center.

A view that also contains the sun stays at the outdoor exposure unless the volume forces the
interior shade.

## 4. Grade the place

A `PostVolume` changes only the `PostOverrides` fields it sets: exposure compensation, white
balance, bloom, saturation. An unbound volume is the level-wide look. A smaller volume with a
higher `priority` overrides it, and `blend_distance` fades across the boundary. A
`PhysicalCamera` exposes from ISO, shutter time, and aperture. Both are in
[Post volumes and the physical camera](../../user-guide/post-volumes.md).

The level's planet (`LevelSettings.atmosphere`: Earth, Mars, or Airless) chooses the sky the sun
sits in. Airless has no haze and no clouds. Planet-shine is a shadowless fill from a sky body.

## 5. Compare with the path tracer

`regia_pt` is an offline ground-truth tracer. It reads the same extracted world as the real-time
frame, shades with the forward BRDF, and writes layered EXR plus an AgX preview. It bakes the
`.fgiv` volumes (`regia bake-gi`) and the lightmaps (`regia bake-lightmaps`, CPU or `--gpu` when
the `lightmap-gpu` feature is on). It does not ship in a game build and runtime crates do not
depend on it. Use it to check energy and contact occlusion. The picture a player sees is the
real-time frame.

Presets inside the tracer (`draft`, `preview`, `final`, `reference`) are bake and comparison
qualities. They are not `QualityTier` and not `RenderProfilePreset::Cinematic`.

## Finished check

1. The sun's shadow reaches the hero set, and `render.lights` lists the directional lights you
   authored.
2. `RenderSettings.gi_look` is the look you chose. With `Baked` or a named `look.baked_gi`,
   moving a static wall does not move the bounce until you bake again. With `Balanced`, the
   volume is ignored and the analytic terms are what you see.
3. `render.get_lightmaps` reports `installed: true` and `attached` equal to the baked receivers
   when the level names a lightmap. A missing file reports `lightmap_not_found`.
4. Standing inside the interior volume, the exposure is near EV 8 or auto exposure has lifted,
   and the exterior through a door still shows the sky.
5. A post volume changes only the overrides you set. Walking out restores the level look.

## Where to read more

- [The default look](../../user-guide/default-look.md) for the order to tune exposure, bloom, and GI.
- [Baking GI from meshes](../../agents/baked-gi-bake.md) and
  [Baked GI from the level](../../agents/baked-gi-maps.md).
- [Display calibration](../../user-guide/display-calibration.md) when the screen, not the level, is
  crushing the shadows.
