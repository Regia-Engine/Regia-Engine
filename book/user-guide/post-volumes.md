# Post volumes and the physical camera

<!-- index: Post volume -->
<!-- index: Physical camera -->
<!-- index: Exposure compensation -->

A **post volume** changes the look of a place: walk into a cellar and the image warms, opens up a
stop and loses a little saturation; walk out and it blends back. A **physical camera** exposes the
image the way a photographer would, from ISO, shutter time and aperture.

Both are components. Design and the exact rules are in
`docs/specs/30-post-volumes-and-physical-camera.md`; agents use the
[post volumes agent page](../agents/post-volumes.md). A volume does not change the quality tier
or the upscaler. Place the lights first ([Light a level](../how-to/picture/light-a-level.md)),
then grade the place with a volume.

## Post volumes

Put a `PostVolume` on an entity with a transform. Its shape is in the entity's local space:

- `PostVolumeShape::Box { half_extents }`: a box (the entity's scale stretches it),
- `PostVolumeShape::Sphere { radius }`,
- `PostVolumeShape::Unbound`: everywhere. Use it for a level-wide look, then let smaller volumes
  override it with a higher priority.

Every setting in `PostOverrides` is optional. A volume only changes the settings it names, so a
volume that sets the white balance leaves exposure, bloom and the rest to the project, the level look
and other volumes.

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
use regia_render::post_volumes::{PostOverrides, PostVolume, PostVolumeShape};

commands.spawn((
    Transform::from_translation(Vec3::new(0.0, 1.5, -20.0)),
    PostVolume {
        shape: PostVolumeShape::Box { half_extents: [6.0, 3.0, 10.0] },
        blend_distance: 3.0, // fade in over 3 m outside the box
        priority: 1,         // beats the level-wide look
        overrides: PostOverrides {
            exposure_compensation: Some(1.0), // one stop brighter
            white_balance_kelvin: Some(9000.0), // warmer print
            saturation: Some(0.9),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    },
));
```

What a volume can set: exposure compensation, the auto-exposure band (lowest and highest EV100) and
the meter's centre weighting, the tonemap operator, contrast and saturation, bloom intensity, the
colour grade (white balance, tint, lift, gamma, gain), vignette, film grain, local exposure, the
strength of the ambient occlusion and a tint on the fog colour. It can also ask the eye for an EV
range (`eye`), which works like an [exposure zone](../agents/exposure-zones.md): the eye adapts
toward the range at the speeds you give.

### How volumes combine

- **Weight.** Inside the shape a volume has its full `weight` (0–1). Outside, it fades from full to
  nothing across `blend_distance` meters with a smooth curve, so a doorway is a transition, not a cut.
  A blend distance of 0 is a hard edge.
- **Priority.** Volumes are applied from the lowest priority to the highest; a higher-priority volume
  at full weight replaces what lower ones did, for the settings it sets.
- **Equal priority.** Overlapping volumes with the same priority are averaged by weight, so the
  result never depends on which entity was spawned first.
- **White balance** blends in mired (1 000 000 / kelvin), which looks even across the range.
- **The tonemap operator** cannot be blended; it switches once, half way through the band.
- **Every camera** uses the volumes at its own position: split-screen views and render-to-texture
  cameras each get their own look.

A scene without volumes renders exactly as before.

### In scene files

Post volumes are saved in `.level.ron` and `.prefab.ron` like any component
(`docs/formats/post-volume-v2.md`; version 1 files load unchanged), and the editor's inspector edits them. Fields you leave out of
`overrides` are not set:

```ron
"regia_render::post_volumes::PostVolume": (
    format_version: 1,
    shape: Sphere(radius: 8.0),
    blend_distance: 4.0,
    overrides: (
        fog_tint: Some((1.0, 0.8, 0.6)),
        ao_intensity: Some(1.5),
    ),
),
```

### Convex hulls, volume LUTs and the gizmo (format v3)

Besides a box, a sphere and unbound, a volume can be a **convex hull** of up to 16 planes. Each plane is
`[nx, ny, nz, d]` in the entity's local space, and the inside is where `n · x <= d`: a slanted ceiling, a wedge of a
stairwell, a floor volume (one plane) that covers everything below a height. The blend band is measured in world
meters from the hull, so it is even all around however the entity is rotated or scaled.

```rust,wrap
use regia_render::post_volumes::{PostVolume, PostVolumeShape};

let attic = PostVolume {
    shape: PostVolumeShape::ConvexHull {
        planes: vec![
            [1.0, 0.0, 0.0, 4.0], [-1.0, 0.0, 0.0, 4.0],
            [0.0, -1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 6.0], [0.0, 0.0, -1.0, 6.0],
            [0.6, 0.8, 0.0, 2.4], [-0.6, 0.8, 0.0, 2.4], // the two roof slopes
        ],
    },
    blend_distance: 1.0,
    lut: Some("looks/attic.cube".into()),
    ..PostVolume::default()
};
```

A volume can name a **colour LUT** (`lut`, a `.cube` or PNG-strip file relative to the project). The LUTs blend by the
volume's weight and priority over the level's LUT, so walking into the attic fades its grade in across the band. This
is **off by default**: turn it on once with `world.resource_mut::<regia_render::PostVolumeLuts>().enabled = true` (or
`render.set_post_volume_luts`); until then volume LUTs are ignored and frames do not change.

The editor gizmo's data is available without a window: `regia_render::post_volume_outline(&volume, &transform)` gives
the shape's and the band's outline as world line segments ([agent page](../agents/post-volumes.md),
`render.post_volume_gizmos`).

## The physical camera

`PhysicalCamera` sits next to a `Camera`. Besides depth of field and motion blur it now has an
exposure mode:

| `exposure` | What sets the exposure |
|---|---|
| `CameraExposureMode::Inherit` (default) | The post stack: auto exposure, or the camera's fixed `exposure_ev`. The lens does not change exposure. |
| `CameraExposureMode::Manual { ev100 }` | A fixed EV100. |
| `CameraExposureMode::Physical` | The lens: `EV100 = log2(N² / t) − log2(ISO / 100)` from `aperture_f_stops`, `shutter_seconds` and `iso`. |

In physical mode the controls behave like a camera's:

- Opening the aperture one stop (f/8 to f/5.6) brightens the image one stop **and** makes the depth of
  field shallower.
- A slower shutter brightens the image **and** lengthens the motion blur: the blur covers
  `shutter_seconds × shutter_reference_fps` of a frame's motion (1/60 s at the default 30 fps is a
  180° shutter).
- A higher ISO brightens the image and changes nothing else.

Sunny 16 (f/16, 1/125 s, ISO 100) is about EV100 15, a sunlit exterior. Exposure compensation still
applies, so a post volume's compensation shifts a physical camera the way a photographer's dial does.

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let camera: Entity = todo!();
use regia_render::{CameraExposureMode, PhysicalCamera};

commands.entity(camera).insert(PhysicalCamera {
    exposure: CameraExposureMode::Physical,
    aperture_f_stops: 4.0,
    shutter_seconds: 1.0 / 60.0,
    iso: 800.0,
    focal_length: Some(0.035), // 35 mm: sets the field of view from the sensor height
    ..PhysicalCamera::default()
});
```

A lens in `Manual` or `Physical` mode wins over the camera's `exposure_ev`. `focal_length` (meters)
sets the vertical field of view from `sensor_height` (Super 35 by default); leave it `None` to keep
the camera's own field of view.

## Lens flare, dirt and chromatic aberration

A `LensEffects` component on a camera (or `PostSettings::lens_effects` for the whole project) adds three lens
artefacts over the HDR image, before exposure and the tonemap (ADR-1863):

- `flare_intensity`: ghost flare. Every pixel brighter than `flare_threshold` (default 4, about two stops over
  display white) casts the ghosts of a traced built-in lens, on the line from the light through the frame centre.
  1 is the energy the lens trace gives; small lamps need more.
- `dirt_intensity`: dust and smudges on the front element, lit by the glare.
- `chromatic_aberration`: colour fringes toward the frame edges (a fraction of the radius, 0.002 is subtle).
- `coating`: 1 is the lens as coated (dim, tinted ghosts), 0 bare glass (bright ghosts).

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let camera = regia_render::Camera::default();
# let transform = Transform::default();
use regia_render::LensEffects;

commands.spawn((camera, transform, LensEffects::CINEMATIC));
```

Every strength 0 (`LensEffects::OFF`, the default) is off and costs nothing. The quality preset must allow the
effects: Desktop High, Desktop Ultra and Cinematic do; the others leave them off. A post volume can change the four
strengths inside it (`lens_flare_intensity`, `lens_flare_threshold`, `lens_dirt_intensity`,
`chromatic_aberration`), for example more dirt glare in a dusty hangar. Agents use `render.lens_effects` and
`render.set_lens_effects` ([agent page](../agents/lens-effects.md)).

## Known limits

- Volumes do not change the camera's depth of field or motion blur; those belong to the camera. They do change
  the lens effects' strengths (above).
- The lens flare is screen-space: a light outside the frame casts no ghosts.
- There is no bloom threshold to override (the bloom is an energy-conserving blend).
- Shapes are boxes, spheres, convex hulls of up to 16 planes and unbound volumes. The outline of a box's or a hull's
  band is the shape pushed out (the real band has rounded corners).
- A sphere's radius scales with the mean of the entity's X and Z scale; scale it uniformly.
