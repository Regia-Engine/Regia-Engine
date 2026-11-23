# Terrain, tunnels and scatter

`regia_terrain` provides heightfields rendered with continuous LOD (CDLOD), material layer
stacks, layer-driven scattering of rocks and vegetation, and mesh tunnels through the ground. It
renders on the runtime path (`docs/specs/26-terrain-vfx-highend.md`). The photoreal material path
and the cooked, streamed data path have their own chapters:
[Photoreal terrain](terrain-photoreal.md) and [Terrain data: cook and stream](terrain-data.md).
Particles, decals and trails moved to [VFX and decals](../systems/vfx.md).

## Heightfields

A heightfield is a 16-bit heightmap (or RON) with a collider. Layer and scatter rules place rocks,
vegetation and decals on it:

```rust
use regia::prelude::*;
use regia_asset::Heightfield;
use regia_terrain::{CdlodConfig, Terrain, TerrainLayers};

fn spawn_terrain(mut commands: Commands) -> Result<(), regia_terrain::TerrainError> {
    // `heightfield` is a cooked `regia_asset::Heightfield`; `Terrain::new` builds the
    // CDLOD quadtree, min/max pyramid and material layer stack from it.
    let heightfield = Heightfield {
        width: 2,
        height: 2,
        samples: vec![0, 0, 0, 0],
    };
    let terrain = Terrain::new(
        &heightfield,
        1024.0, // world_width (meters)
        100.0,  // height_scale
        0.0,    // height_offset
        CdlodConfig::default(),
        4,      // root_level (quadtree depth)
        TerrainLayers::default(),
    )?;
    commands.spawn(terrain);
    Ok(())
}
```

Layers (grass, rock, snow…) are driven by slope/height/noise rules; scatter rules stamp
instances from the layer (`docs/formats/terrain.md`). The `TerrainPlugin` selects LOD per frame
from the `TerrainView` (camera) and draws through the render graph.

### Terrain collision

Build the terrain's physics collider with `TerrainMap::collision_heightfield`. It lays the heights
out the way `ColliderShape::HeightField` reads them (Z fastest) and cuts the terrain's holes. Its
triangles are the same as the finest drawn grid:

```rust,wrap
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_scene::Transform;
# use regia_core::math::Vec3;
# use regia_ecs::prelude::{Commands, Query};
# use regia_terrain::Terrain;
fn attach_terrain_colliders(mut commands: Commands, terrains: Query<&Terrain>) {
for terrain in &terrains {
let c = terrain.map.collision_heightfield(terrain.holes.as_ref());
commands.spawn((
    Transform::IDENTITY, // terrains are centered on the world origin
    RigidBody::static_body(),
    Collider {
        shape: ColliderShape::HeightField { nx: c.nx, nz: c.nz, heights: c.heights, scale: Vec3::from(c.scale) },
        ..Collider::default()
    },
));
}
}
```

Do not fill `ColliderShape::HeightField` from `TerrainMap::heights()` directly: that array is X
fastest, and the collider comes out mirrored about the X = Z diagonal. After sculpting, rebuild
the collider. The `terrain.parity` agent method ([terrain parity](../agents/terrain-parity.md))
checks LOD seams, the collider and the saved heights in one call.

## Tunnels

A tunnel through a heightfield is a hole mask, an interior mesh and a trimesh collider
(`MeshTunnel`). The [agent guide's tunnels page](../agents/tunnels.md) covers the construction and
how to verify the bore with raycasts.

## Decals on terrain

Scorch marks, landing-pad paint and tire ruts project onto terrain through the decal system;
the terrain writes itself into the receiver buffer as layer 0, so a decal's receiver mask can
include or exclude the ground. See [VFX and decals](../systems/vfx.md).

## Water

A terrain can carry one shoreline water plane: `Terrain::water`, a `WaterPlane`. It is a flat
surface at a fixed `level` with small ripples. Its color depends on depth. There is no simulation.
The format is in `docs/formats/terrain.md` § 3, and ADR-1237 has the shading model:

- the surface reflects the sky more at grazing angles (Fresnel);
- the bed shows through shallow water, and deep water shows `deep_color` (absorption along
  the refracted view ray);
- the opacity fades in over the last 0.2 m at the waterline;
- the foam band is `foam_width` meters wide, measured across the beach.

The offscreen terrain renderer (`render_offscreen`, `samples/terrain_lab`) draws the plane and
refracts the picture through it. `TerrainGpu::set_water` swaps the plane on a built terrain. In
the runtime frame, `WaterPlugin` also draws that plane as a water body unless `terrain_planes`
is false. The agent calls `terrain.water` and
`terrain.set_water` probe and edit the plane ([terrain water](../agents/terrain-water.md)).
`WaterPlane::probe` gives depth, distance to the shore and foam at a point, for gameplay
queries.

## Cook and preview

Terrain is authored as text and images and cooked like other assets; `regia preview` shows a mesh
asset on a neutral stage, and `seeing.screenshot` with `target: "asset"` renders it headless for
goldens.
