# Shadow LOD hint

A mesh with a LOD chain (`MeshLodChain`) picks two levels each frame: one for its colour draw
and one for its shadow draws. The shadow level switches earlier than the colour level (the
tier's `mesh_lod_shadow_scale`), so far shadows cost fewer triangles. A **shadow LOD hint**
(`regia_render::ShadowLodHint`, ADR-1622) bounds that shadow level for one object:

- `max_lod` caps how coarse the shadow may get. Use it on a hero object whose coarse shadow
  pops or self-shadows badly. `max_lod: 0` keeps the shadow at full detail.
- `min_lod` keeps the shadow at or below a level of detail. Use it on a busy prop whose fine
  shadow costs more than it shows. `min_lod` equal to `max_lod` fixes the shadow to one level.

The selector picks the shadow level as usual (with its hysteresis), then clamps it to
`min_lod..=max_lod` and to the chain's last level. When the two bounds conflict, `min_lod`
wins. The colour level never changes. When the clamped shadow level equals the colour level
and no cross-fade runs, the mesh draws once, as it does without a hint. An entity without a
hint, or with the neutral hint (`max_lod` none, `min_lod` 0), selects exactly as before.
Shadows the visibility buffer's clusters cast (`cluster_shadows`) pick their detail per texel
and ignore the hint.

In code:

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let (tank, crate_stack): (Entity, Entity) = todo!();
use regia_render::ShadowLodHint;

commands.entity(tank).insert(ShadowLodHint::cap(1)); // never coarser than LOD 1
commands.entity(crate_stack).insert(ShadowLodHint::fixed(2)); // always LOD 2
```

## `render.shadow_lod_hints`

No params. Lists every entity with a hint.

```json
{ "method": "render.shadow_lod_hints" }
```

Returns `{ entities: [{ entity, max_lod, min_lod, neutral, chain_levels }] }`. `max_lod` is
`null` when there is no cap. `chain_levels` is the entity's LOD count, or `null` when it has
no `MeshLodChain` (then the hint changes nothing).

## `render.set_shadow_lod_hint`

Params: `entity` (required), and any of `max_lod` (0 to 254, or `null` to remove the cap),
`min_lod` (0 to 254, or `null` to remove the floor) and `clear` (remove the hint). A field you
leave out keeps its current value.

```json
{ "method": "render.set_shadow_lod_hint", "params": { "entity": 4294967301, "max_lod": 1 } }
{ "method": "render.set_shadow_lod_hint", "params": { "entity": 4294967301, "min_lod": 2, "max_lod": 2 } }
{ "method": "render.set_shadow_lod_hint", "params": { "entity": 4294967301, "clear": true } }
```

Returns `{ before, after, note }`: the entity's hint before and after (`null` without one), and
a note that says whether the entity has a LOD chain. The hint applies from the next extracted
frame. Errors: `F0002` without `entity`, without any of the three fields, or with a value out
of range; `F0003` when the entity does not exist.

Check it worked:

1. `render.shadow_lod_hints` lists the entity with the bounds you set, and `chain_levels` is not
   `null` (the entity has a LOD chain for the hint to act on).
2. `render.mesh_lod` with the object's bounding radius and stored screen sizes gives `switch`
   distances: past `shadow_m` the shadow would step to a coarser level.
3. Put the camera beyond that distance and take a `seeing.screenshot` with and without the hint
   (`clear: true` removes it). With `max_lod: 0` the shadow keeps the LOD 0 silhouette; with
   `min_lod` it shows the coarser level even close up.
