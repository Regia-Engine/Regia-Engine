# Mesh tunnels

A tunnel through a heightfield is three pieces (`regia_terrain::MeshTunnel`):

1. A `HolesMask` ellipse on the heightfield so the surface (and its collider samples) drop out
   of the bore.
2. An imported or collision-derived render mesh for the interior.
3. A trimesh collider (`ColliderShape::trimesh_from_triangles` on
   `MeshTunnel::collision_trimesh`).

```rust
use regia::physics::ColliderShape;
use regia_terrain::{CollisionProvenance, HoleEllipse, MeshTunnel, TunnelDetail};

fn build_tunnel() -> Result<(), regia_terrain::MeshTunnelError> {
    // A triangle fan for the bore (a real tunnel imports or sim-supplies these).
    let positions: Vec<[f32; 3]> = vec![[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [2.0, 0.0, 3.0]];
    let indices: Vec<u32> = vec![0, 1, 2];
    let provenance = CollisionProvenance {
        source_asset: "collision_tunnel_north".into(),
        triangle_count: 1,
        note: "sim collision mesh".into(),
    };
    let tunnel = MeshTunnel::from_collision(
        &positions,
        &indices,
        HoleEllipse {
            center: [0.0, -20.0],
            radius: [5.0, 4.0],
            rotation_radians: 0.0,
        },
        TunnelDetail::default(),
        provenance,
    )?;
    // `None` when the indices are not triples into `vertices` — validated data is always `Some`.
    let shape = ColliderShape::trimesh_from_triangles(
        &tunnel.collision_vertices,
        &tunnel.collision_indices,
    );
    let _ = shape;
    Ok(())
}
```

Spawn that trimesh **and** a `ColliderShape::HeightField` cut by the same mask:
`HolesMask::cut_heights` sets the samples in (or, with a margin, next to) the hole to NaN, and
the collider removes every cell touching a NaN sample. A horizontal ray misses a flat plateau,
so the ground beside the hole has to rise across the ray.

## Cutting mouths and ramp trenches

`TerrainSharedDraw::set_holes(Some(&mask))` cuts any footprints through the drawn terrain
(ADR-088): `HoleShape::Rect`, `Polygon`, `Ellipse`, a coverage `Raster` in terrain space, or
`HoleShape::segment(from, to, width)` for an open-cut ramp (a DO27 mouth is about 20 m × 25 m).
Inside, nothing is drawn in the color, depth, velocity, decal-receiver or shadow passes, so
the cut casts no shadow while the ground around it still shades the tube below. The edge is
per pixel and fixed in world XZ at every LOD. `rendered_heights` and the `RenderedSurface`
answer NaN inside, and scatter is not drawn over it.

From the agent protocol (insert `draw.holes_handle()` as a resource first):

```json
{"method": "terrain.set_holes", "params": {"segments": [{"from": [-142.8, -286.0], "to": [-170.0, -256.0], "width_m": 20.0}]}}
```

`terrain.holes` lists the cut shapes and `pending` until the next frame applies them.

## The map's holes file (`<map>.holes.ron`)

A map's holes live in `<map>.holes.ron` next to the map (`maps/switchbacks.level.ron` →
`maps/switchbacks.holes.ron`, `regia_terrain::holes_ron_path`). The file is a serialized
`HolesMask` with a `version` (format v2, `docs/formats/terrain.md` § 5): at most 64 shapes in
all (ellipses and named holes included), rasters at most 2048 texels a side. A bare
`ron::to_string(&mask)` without the `version` field loads as version 1; a file from a newer engine
is refused. Version 2 adds named holes that gameplay opens and closes at runtime
(`terrain.set_hole`, [terrain runtime](terrain-runtime.md)); a version 1 file loads unchanged.

```ron
(
    version: 1,
    shapes: [
        polygon(points: [(-15.0, -9.0), (14.0, -10.0), (16.0, 8.0), (0.0, 12.0), (-14.0, 9.0)]),
        rect(center: (40.0, 2.0), half_extents: (10.0, 12.5), rotation_radians: 0.7),
        ellipse((center: (-40.0, 0.0), radius: (6.0, 4.0), rotation_radians: 0.0)),
    ],
)
```

Load it and cut both the drawn terrain and the collider:

```rust,wrap
# use regia_terrain::TerrainSharedDraw;
# let draw: &mut TerrainSharedDraw = todo!();
# let map_path: &std::path::Path = todo!();
# let (mut collider_heights, spacing): (Vec<f32>, f32) = todo!();
# let sample_xz = |i: usize| [i as f32, 0.0];
let mask = regia_terrain::load_holes_ron(&regia_terrain::holes_ron_path(map_path))?;
draw.set_holes(mask.as_ref());
if let Some(mask) = &mask {
    mask.cut_heights(&mut collider_heights, spacing * 0.5, |i| sample_xz(i));
}
```

`HolesMask::to_holes_ron` writes one. From the agent protocol, `terrain.load_holes` cuts a
document on the next frame, from its text or a project path:

```json
{"method": "terrain.load_holes", "params": {"path": "maps/switchbacks.level.ron"}}
```

## What sees the holes

- **Drawing and shadows:** every pass and cascade (above).
- **Picking and engine rays:** `regia_terrain::pick_terrain(world, origin, direction, max)`
  and `raycast_with_holes` pass through a hole, like the physics heightfield: below the
  surface there is no rock, so a ray down a mouth hits nothing more of the terrain (test the
  tunnel meshes yourself). They read the mask the draw applied (`TerrainHoles`, once a draw has
  applied one) and use `Terrain::holes` only when no mask has been applied. The editor's picks call
  them, so a tunnel mesh under the
  mouth can be clicked. On the agent protocol:

  ```json
  {"method": "terrain.raycast", "params": {"origin": [0, 80, 0], "direction": [0, -1, 0]}}
  ```

  `hit` is `null` when the ray falls through the mouth; `hit.holes` counts the shapes the hit
  terrain is cut by.
- **GPU irradiance probes:** the probe rays read the draw's hole windows. A probe in a mouth is
  open interior space, not a buried probe; a ray down a mouth that runs into the ground beside
  it below the surface meets the shaft's rock wall. A new mask wakes settled probes.
- **Path tracer and the GI bake:** the shared draw's reference heightfield is NaN inside the
  holes and the tracer drops every cell touching a NaN sample (the cut is up to one cell wider
  than the drawn one). `PtHeightfield::cut_holes` cuts a heightfield you build yourself, and a
  `regia bake-gi` description takes `holes: Some("map.holes.ron")` on its terrain. A baked
  terrain-layer column over a mouth starts at the lowest rim height around it.

A bore with no local lights is black at outdoor exposure. Spawn the lights along
the mesh and an `InteriorVolume`, or an `EnclosureScene` so extraction runs
`detect_enclosure`. The shade drops sky IBL and holds the 40 lux floor at EV 8.
Fit the volume to the bore's inner faces: walls, floor and roof on a face (or up to 2 cm outside
it) shade as inside. A headlight placed exactly at the camera is fine: it lights the cone
without black (NaN) pixels.
`build_interior_lighting` traces one bounce into a probe grid (misses stay black)
and the frame takes that field plus one reflection probe. Desktop High keeps 256
of those lights; a shorter list is what a Deck profile will warn about. See
[high-end lighting](high-end-lighting.md).

## Rays

- Into the mouth, stopping before the far end: misses terrain and walls.
- Into solid ground beside the hole: hits the heightfield.
- Down the bore through a **closed** far cap: hits the cap. The stock box helper is an open
  tube (mouth and exit both open). Add the far-end triangles when the tunnel is a dead end.
  Wind the cap so its normal faces back toward the mouth.

`physics.raycast` on the agent protocol hits the same colliders once `PhysicsPlugin` has
stepped.

### More `terrain.set_holes` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `clear` (boolean): Remove every hole (with no shapes or segments).

### More `terrain.raycast` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `max_distance` (number): Search distance, metres (default 10000).
