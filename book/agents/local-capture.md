# Local reflection captures

A metal object in a studio reflects the environment, but not the plinth under it or the object next to
it: the environment is at infinity. A local reflection capture is an HDR reflection of what stands
around a point, rendered once or on demand and read by the image-based specular. It needs no GI probe
volume, and it works on every quality tier that draws the image light.
A frame with a baked capture draws the full forward shader on a Mobile tier that asks for the lite path, at the
full shader's cost on a tile GPU (`tier` in `render.get_forward_lite` shows it).

A renderer holds up to 32 captures (`MAX_LOCAL_CAPTURES`) and a frame reads at most four of them, the nearest
within the tier's budget (ADR-2860). Saving captures with a level, the `.fcap` bake and the selection are on
[Reflection captures](reflection-captures.md).

## From an agent

1. `render.place_local_capture { "id": 1, "center": [0.0, 1.0, 0.0], "half_extent": [3.0, 2.0, 3.0] }`
2. `render.bake_local_capture { "id": 1 }`. The bake runs on the next frame (six faces, then the filter).
3. Poll `render.list_local_captures` until that capture's `baked` is `true`. `place_local_capture` does not report a bake it kept: its own `baked` field is always `false`. `list_local_captures` is the one that says whether a previous bake is still held.
4. `play.step`, then `seeing.screenshot`: metal around the capture now shows the plinth and its
   neighbors.

| Method | Parameters | Result |
|---|---|---|
| `render.place_local_capture` | `id`, `center`, `half_extent`, `weight?`, `fade?` | `{ id, baked, captures }`. `baked` in this reply is always `false`, including when the renderer still holds a bake of this id. |
| `render.bake_local_capture` | `id?` (all when omitted) | `{ queued }` |
| `render.list_local_captures` | none | `{ captures: [...], max }` |
| `render.remove_local_capture` | `id` | `{ removed, captures }` |

Each listed capture has `id`, `center`, `half_extent`, `weight`, `fade`, `baked`, `pending`, `bakes` and
`coverage`, the share of its directions that saw a surface (0 means it saw only the environment, and
reflects nothing new).

Placing the same id again moves that capture: bake it again to see the move. Removing a capture frees
its layer; the others keep their layers and maps, and the next frame's selection fills the free layer.

Errors: `F0002` for a bad value or a 33rd capture, `F0003` for an unknown id or an app without a
renderer.

## From a host

```rust,wrap
# use regia_render::{FrameRenderer, RenderDevice};
# let renderer: &mut FrameRenderer = todo!();
# let device: &RenderDevice = todo!();
# let scene: regia_render::FrameScene = todo!();
use regia_render::LocalCaptureDesc;

renderer.place_local_capture(LocalCaptureDesc::new(1, [0.0, 1.0, 0.0], [3.0, 2.0, 3.0]))?;
let info = renderer.bake_local_capture(&device, &scene, 1)?;
assert!(info.baked);
```

Or put a `LocalCapture` component on an entity with a transform: the runtime places it at the entity,
queues a bake when it appears or moves, and removes it with the component.

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_render::LocalCapture;
commands.spawn((Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)), LocalCapture::new(1, [3.0, 2.0, 3.0])));
```

`bake_local_capture` blocks while it renders six frames; the host's frame state (temporal history, shadow
placements, the previous view) is kept, as for `capture_reflection_probe`. The runtime bakes what
`request_local_capture_bake` queued at the start of its next window frame.

## Placing one

- **`center`** is where the capture sees from. Put it where the reflecting object is. A capture inside
  a closed single-sided mesh sees past it, because its back faces are culled, which is what a sphere on a
  plinth wants: the capture at the sphere's center sees the plinth and not the sphere. A double-sided mesh
  blocks the view.
- **`half_extent`** is the parallax box: the size of the space the reflection stands in. A reflection
  ray from a surface is carried to this box and the capture is read in that direction, so a point on the
  object sees the nearby wall where it really is. A box that fits the room or the set is right. A box much
  larger than the set loses the correction, a box much smaller than the object bends it too far.
- **`weight`** blends against the environment, 1 by default.
- **`fade`** is how far outside the box the capture fades out, a quarter of the largest half extent by
  default. Inside the box it has full weight. Overlapping captures share by weight.

## What is captured

The faces are linear HDR frames of the scene with its sun, its local lights and the environment, at
256 × 256 per face, without GI, screen-space reflections, TAA or other captures, so a capture never
reflects itself. They are packed into a 512 × 256 map and GGX-filtered with the same chain as a studio
environment.

The map also keeps which directions saw a surface. Where a capture saw only sky, a mirror keeps the
sharper environment; a rough lobe that is half surface blends half with the environment. The studio
background never appears in a capture: it shows the lighting environment.

The ground plane of `render.set_environment_ground` lights the capture's surfaces like any others. It
is not drawn, so a capture does not see it as a surface.

A capture costs about 1.4 MB of CPU memory for its kept bake; the array texture is 6 MB once the first one bakes
(four layers), up to four texture
samples per pixel inside the boxes, and nothing per frame beyond that. Nothing updates it: a moved object
needs another bake. A capture only feeds the specular image light, not the diffuse term, and not the
clearcoat lobe.

The older `ReflectionProbe`s (`capture_reflection_probe`) are coarse 64 × 32 display-referred images that
the screen-reflection passes blend on the tiers that run them; they are unchanged.
