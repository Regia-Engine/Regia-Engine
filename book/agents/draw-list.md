# Keyed draws (draw list)

A host that keeps its own scene (a lockstep sim mirror, a crowd, thousands of props or rocks) can
draw it without spawning one ECS entity per drawn part. It fills the `regia_render::DrawList`
resource; `extract_frame` appends every visible item to the frame after the mesh entities
(ADR-8087).

## Why not entities

Mirroring a scene into entities costs a spawn per new part, a `Transform`, `GlobalTransform` and
`MeshRenderer` write per moving part per frame, deferred commands to add and remove components,
and a pool of spare entities so a despawn does not mint a new object key (which would reset the
draw's motion history). With the draw list there is no entity, no command and no write for a draw
that did not move. 9,000 keyed draws spawn no entity (`drawlist_9k_keyed_draws_spawn_no_entities`).

## Using it from a host

<!-- ignore-reason: host-side sketch; MySim and MyMeshes stand for the host's own sim and mesh bank -->
```rust,ignore
use std::sync::Arc;
use regia_core::math::Mat4;
use regia_render::{KeyedDraw, DrawKey, DrawList};

fn mirror(mut list: ResMut<DrawList>, sim: Res<MySim>, bank: Res<MyMeshes>) {
    // Per frame: submit what is drawn now; every key not submitted is dropped.
    list.begin_sync();
    for unit in &sim.units {
        for (part, mesh) in bank.parts(unit).iter().enumerate() {
            let model = Mat4::from_rotation_translation(unit.rotation, unit.position);
            list.submit(
                DrawKey::pair(unit.id, part as u64),
                KeyedDraw::new(Arc::clone(&mesh.mesh), Arc::clone(&mesh.material), model),
            );
        }
    }
    list.end_sync();
}
```

- **Keys.** A `DrawKey` is any `u64` the host owns. `DrawKey::pair(owner, part)` mixes two numbers
  into one. The same key is the same draw: inserting under it replaces the item.
- **Object key.** The renderer's per-object cache slot and temporal history (motion vectors, TAA)
  is `DrawKey::object_key()`, a fixed function of the key. A draw keeps its history across frames,
  across removal and re-insertion, and across runs.
- **Order.** Items are extracted in ascending key order, so the frame is the same whatever order the
  host inserted them in.
- **Mesh and material.** The GPU caches key on `Arc` identity. Keep one `Arc` per asset and hand the
  same clone to every draw of it, or every frame uploads again.
- **Retained or per frame.** `insert`, `set_transform`, `set_visible`, `get_mut` and `remove`
  change single draws; `begin_sync` / `submit` / `end_sync` (or `sync` over an iterator) mirror a
  whole frame and report what was inserted, updated and removed.
- **What an item carries.** `KeyedDraw::new(mesh, material, model)` draws what a
  `MeshRenderer::new` entity at the same transform draws (layer 0, all shadows, a GI proxy). The
  `with_*` methods set the layer, visibility, shadow policy, skin palette, decal layer, GI proxy,
  `is_static` and a second UV set.

Draws from the list are not entities. `Visibility`, layer parking, render overrides, glass, morph
targets and `ViewVisibility` do not apply to them; the item's `layer` and `visible` are its own.
Frustum and occlusion culling, shadows, skinning from a palette, decals and mesh LOD chains work as
for any extracted mesh.

## LOD chains and cross-fades

A keyed draw takes the same `MeshLodChain` an entity does (ADR-8457). Build the chain once per asset
(`MeshLodChain::from_cooked` or `from_cooked_asset`), keep it in an `Arc`, and hand the same clone
to every draw of that asset:

<!-- ignore-reason: host-side sketch; `part`, `unit`, `model` and `list` come from the host mirror system above -->
```rust,ignore
let item = KeyedDraw::new(Arc::clone(&part.mesh), Arc::clone(&part.material), model)
    .with_lod_chain(Arc::clone(&part.lod_chain))
    .with_lod_bias(1.0)                          // above 1 keeps detail longer
    .with_shadow_lod_hint(ShadowLodHint::cap(1)); // optional: never a coarser shadow than LOD 1
list.submit(DrawKey::pair(unit.id, part_index), item);
```

- **Same rule as an entity.** `select_mesh_lods` picks the level by projected size times the bias
  and the tier scales, holds it inside the `MESH_LOD_HYSTERESIS` band, dither-cross-fades a switch
  over `MESH_LOD_FADE_FRAMES` frames, and draws the shadow from the coarser shadow LOD. One mesh drawn
  as an entity and as a keyed draw gives the same levels and fade factors on every frame
  (`drawlist_item_crossfades_like_an_entity`).
- **Memory per key.** The hysteresis and fade state is kept per `DrawKey`, in a map apart from the
  entities, so a mirror host that re-submits a new `KeyedDraw` value every frame keeps the level, and a
  key whose number equals an entity's bits shares nothing with it. A key missing for a frame comes
  back as a fresh pick (no fade from a stale level), as an entity does (`drawlist_lod_memory_is_per_key`).
- **History.** The main draw keeps the key's object key across switches; the outgoing level of a fade
  and a separate shadow-LOD draw carry it with the same tags an entity's do, so TAA and motion vectors
  follow the draw (`a_keyed_draw_keeps_its_object_id_across_frames`).
- **Skinned items** draw their own mesh, as skinned entities do.

## Shadow policy

`KeyedDraw::with_shadow(ShadowCast::Local { .. })` gives a list draw a local shadow. For an ECS
`MeshRenderer` entity the same policy is the `MeshShadowCast(ShadowCast)` component (default `All`
without it; `NotShadowCaster` wins over it). Both reach the extracted frame, and through a LOD chain
the separate shadow draw keeps the policy while the colour draw casts nothing
(`shadow_cast_policy_component_reaches_extraction`).

## What the list holds: `render.draw_list`

```json
{ "method": "render.draw_list", "params": { "limit": 2 } }
```

returns

```json
→ {
  "present": true,
  "items": 9000, "hidden": 0, "skinned": 0,
  "chained": 9000, "local_casters": 8200, "shadow_off": 600,
  "extracted": 9000, "extractions": 41,
  "lod": { "selected": 9000, "color_lods": [1200, 3100, 4700], "shadow_lods": [300, 2100, 5800],
           "crossfading": 37, "selections": 41 },
  "layers": [{ "layer": 0, "items": 9000 }],
  "listed": 2, "truncated": true, "limit": 2,
  "draws": [
    { "key": "0001a2b3c4d5e6f7", "object_key": "9c41f0e2d3b4a596", "layer": 0, "visible": true,
      "shadow": "all", "joints": 0, "lod_levels": 3, "translation": [-20.0, -18.0, -20.0] }
  ]
}
```

- `items`, `hidden` (items with `visible` false) and `skinned` (items with a palette) count the list.
- `extracted` is how many meshes the last frame's extraction appended (hidden items and meshes
  without triangles are not), and `extractions` how many extractions read the list.
- `chained` counts items with a LOD chain of two or more levels, `local_casters` items whose shadow
  is `local`, and `shadow_off` items that cast none.
- `lod` is what the last LOD selection did with the chained items: `selected` (drawn rigid chained
  items it placed; none without a camera), `color_lods` and `shadow_lods` (how many draw each LOD
  index, LOD 0 first; a shadow is counted only when the item casts), `crossfading` (draws fading
  between two levels this frame) and `selections` (selection runs so far).
- `layers` counts items per render layer.
- `draws` lists up to `limit` items (default 64, at most 4096) in key order. `key` and
  `object_key` are 16-digit hex strings because they do not fit a JSON number. `shadow` is `all`,
  `off` or `local`; `joints` is the palette length; `lod_levels` is the chain's level count (0
  without one); `translation` is the model matrix's.
- `present` is false (and every count 0) when the app has no `DrawList`.

A bad `limit` is the universal invalid-params error `F0002`. The method is read only.
