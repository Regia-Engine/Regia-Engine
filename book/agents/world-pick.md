# World pick: rays over terrain and layers

`world.pick` casts a ray through the rendered world and reports the nearest surface and the `MapLayer` it
belongs to. It runs on the CPU against terrain heightfields and mesh renderers
(`regia_pick::world_pick`); it needs no GPU and no window.

- Terrain is layer 0. A mesh entity takes the `MapLayer` on itself, else its nearest ancestor's, else 0.
- A ray over a terrain hole (a tunnel mouth cut by `terrain.set_holes`) passes through to what lies below.
- `origin` and `point` are local coordinates, the `GlobalTransform` frame. After a floating-origin rebase that is not world space. The terrain heightfield stays world-anchored: the march adds `OriginOffset` and subtracts it from the point it returns. A zero offset makes the two frames the same.
- Skinned meshes are cast where their joint palette draws them, not in bind pose. The default is
  four-weight linear blend. Dual-quaternion blending and an eight-influence table follow the
  skinning options the renderer resolves: the table is read only from the mesh entity, and only
  when its length matches the mesh.
- A mesh with morph targets and weights is cast in the deformed pose, using the same active
  weights the renderer applies (game mode keeps the sixteen largest). Without weights it stays
  in bind pose. Skinning then runs on that deformed mesh.
- `cull` applies to mesh triangles only. Both mesh faces count unless `cull` is `back` (front faces only). A terrain hit is only a crossing from above, whatever `cull` is.

```json
{ "method": "world.pick",
  "params": { "origin": [0, 50, 0], "direction": [0, -1, 0], "layers": [0, 1] } }
```

returns

```json
{ "hit": { "entity": 4294967301, "name": "deck", "layer": 1, "surface": "mesh",
           "point": [0, 5, 0], "normal": [0, 1, 0], "distance": 45, "triangle": 4 },
  "count": 1 }
```

`all_layers: true` returns `{ hits: [...], count }`, the nearest hit of each accepted layer, nearest first: what the
ray crosses through stacked floors. `terrain` and `meshes` switch each kind off, `max_distance` caps the ray,
`include_hidden` also tests entities whose own or inherited (hidden-ancestor) `Visibility` is `Hidden`, and `skip` lists entity ids to ignore.

From Rust:

```rust,wrap
# let world: &mut regia::prelude::World = todo!();
# let (origin, direction) = (regia_core::math::Vec3::ZERO, regia_core::math::Vec3::Z);
use regia_pick::{PickFilter, world_pick};
let ray = regia_core::math::Ray::new(origin, direction);
let hit = world_pick(&world, ray, &PickFilter::on_layers(&[1, 2]));
```

To verify a layering: `world.pick` straight down at a point over each floor, once without `layers` (the top floor) and once
per `layers: [n]`, then `terrain.holes` to confirm a mouth is cut where the ray should pass.

`origin` and `direction` are required finite 3-vectors (array or `{x,y,z}`); direction cannot be zero.
Optional filters are `layers` (u8 ids), `all_layers`, `terrain`, `meshes`, `include_hidden`, `skip`
(entity-bit ids), `max_distance` (positive distance cap; 0 is refused), and `cull` (`none` or `back`, mesh triangles only). Invalid
shapes or values return `F0002`. Workflow: cast the default ray, narrow with `layers`, then request
`all_layers: true` to inspect stacked surfaces.

Captured failing call (lavapipe; a zero direction is refused):

```console
$ regia agent call world.pick '{"origin":[0,0,0],"direction":[0,0,0]}' --project samples/arena
error: method call failed: `direction` must not be zero (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```

## A tunnel mouth

Put a tunnel's meshes on their own `MapLayer`. A ray into the mouth passes the terrain hole and hits the
tube on that layer; filtered to layer 0 it falls through the hole and hits nothing. The tunnel acceptance
scene checks this per pixel of an approach frame against a triangle oracle
(`a_pick_ray_into_the_mouth_hits_the_tube`, [Tunnel acceptance scene](tunnel-scene.md)).

With mesh layers (`TerrainMeshLayers` on the terrain, [Terrain mesh layers](terrain-mesh-layers.md)) the hole is cut on
its lattice triangles as drawn, and the mouth's band is terrain: a ray into the mouth hits the band (layer 0) or the
layer's chunk meshes on their own `MapLayer`, never the cut ground (`a_pick_ray_into_the_mouth_hits_the_layer`).
