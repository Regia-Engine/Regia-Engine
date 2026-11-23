# Render overrides and surface picks

Two editor-side tools for asset apps (Tincal engine requests F5 and F7): change how one entity
draws without cloning its material, and find the exact surface point under the cursor.

## Render overrides

```rust,wrap
# use regia::prelude::*;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let (root, selected, candidate, socket, skeleton): (Entity, Entity, Entity, Entity, Entity) = todo!();
use regia_render::RenderOverride;

// Dim everything but the selected part: ghost the model's root, stop it on the part.
world.entity_mut(root).insert(RenderOverride::ghost(0.25));
world.entity_mut(selected).insert(RenderOverride::default());

// Tint, glow, x-ray, unlit.
world.entity_mut(candidate).insert(RenderOverride::tinted([0.4, 0.8, 1.0, 1.0]));
world.entity_mut(socket).insert(RenderOverride { emissive: Some([1.0, 0.6, 0.0]), ..Default::default() });
world.entity_mut(skeleton).insert(RenderOverride { xray: true, unlit: true, ..Default::default() });
```

| Field | Effect |
|---|---|
| `tint: Option<[f32; 4]>` | Multiplies the base colour (linear 0-1); alpha multiplies the coverage. |
| `emissive: Option<[f32; 3]>` | Adds a display-referred colour (0-1), divided by the exposure, so it reads the same in any scene. |
| `alpha: Option<f32>` | Coverage. Below 1 the entity draws alpha-blended (sorted with other blended draws, no shadow). |
| `xray: bool` | Drawn blended with the depth test off: visible through what is in front. |
| `unlit: bool` | The (tinted) base colour without lighting. |

The override applies to the entity and every descendant (`ChildOf`) without one of its own; a
`RenderOverride::default()` stops the inheritance. In the editor viewport, a model's hidden part
entities take their owner's override.

**No material clone.** The override rides the instance's object data (two unused lanes next to
the model matrix), so every entity keeps sharing its material `Arc`, GPU bind group and instanced
batch. Changing a tint every frame rewrites the instance's object data, with no new buffer or
bind group. A ghost or x-ray instance leaves the GPU-driven batch for the blend pass while it is
on; a tint does not.

**Hosts.** `RenderPlugin` extracts the overrides into `ExtractedRenderOverrides` after
`extract_frame` and the window and texture-view renders apply them. A host that renders an
`ExtractedFrame` itself calls, every frame:

```rust,wrap
use regia_render::{ExtractedRenderOverrides, FrameRenderer};

fn sync_overrides(renderer: &mut FrameRenderer, overrides: &ExtractedRenderOverrides) {
    renderer.set_render_overrides(overrides.keys.iter().copied()); // empty clears them
}
```

Limits: x-ray is not applied to material-graph surfaces (they draw blended with the depth test);
the visibility-buffer path draws overridden instances forward instead.

## Surface picks

<!-- ignore-reason: requires a rendered editor pane and its GPU id buffer; regia_editor (desktop-only) is not a dependency of the book's snippet crate -->
```rust,ignore
use regia_editor::picking::{FaceCull, raycast_entity, raycast_mesh, raycast_world};
use regia_editor::viewport::pick_hit_in_frame;

// Under a pixel of a rendered pane (render it with `ids: true`).
if let Some(hit) = pick_hit_in_frame(world, &frame, pixel, FaceCull::None) {
    place_socket(hit.entity, hit.position, hit.normal);
}
// The same through the id buffer directly.
let hit = frame.ids.as_ref().and_then(|ids| ids.hit(world, &frame.view, x, y));
```

`PickHit` carries `entity` (the mesh entity hit), `owner` (what a click selects), `triangle`,
`indices`, `barycentric`, `position`, `normal` (the triangle's front side, world space),
`smooth_normal`, `distance` and `front_face`.

The id buffer names the entity under the pixel; a CPU ray cast (Möller-Trumbore) against that
entity's triangles, and its parts', refines it. Node transforms come from `GlobalTransform`.
Skinned meshes are cast where they are drawn (the current `SkinJoints` palette, then the model
matrix, as the renderer does); the id pass draws them in bind pose, so they are always cast too.
If the named entity's triangles miss (an edge pixel), every visible mesh is cast. `raycast_mesh`,
`raycast_entity` and `raycast_world` take any ray (brush strokes, tools without a pane).

Front and back follow the renderer's winding: a mirroring model matrix (negative scale) turns an
outside face into a back face, as it does on screen.
