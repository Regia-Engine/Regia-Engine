# Terrain, tunnels and scatter

<!-- index: Terrain -->
<!-- index: Heightfield -->
<!-- index: CDLOD -->
<!-- index: Terrain collision -->
<!-- index: Tunnel -->
<!-- index: Scatter -->

`regia_terrain` provides heightfields rendered with continuous LOD (CDLOD), material layer
stacks, layer-driven scattering of rocks and vegetation, and mesh tunnels through the ground. It
renders on the runtime path (`docs/specs/26-terrain-vfx-highend.md`). The photoreal material path
and the cooked, streamed data path have their own chapters:
[Photoreal terrain](terrain-photoreal.md) and [Terrain data: cook and stream](terrain-data.md).
Particles, decals and trails moved to [VFX and decals](../systems/vfx.md).
The task procedure is [Author a terrain](../how-to/world/terrain.md).

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

## Water bodies, swell, and the ocean

<!-- index: Water body -->
<!-- index: Water plane -->
<!-- index: Swell -->
<!-- index: FFT ocean -->
<!-- index: Floater -->

A level can carry water that is not the terrain's one shoreline plane.
`regia_terrain::water_body` (`docs/specs/water.md`) draws through the frame's translucent layer:

- `WaterShape::Plane` is a lake or a sea: a rectangle, at most 16 bodies in one frame.
- `WaterShape::Ribbon` is a stream or a river along a centerline. Speed follows Manning's equation.
- `WaterLook` is absorption, scattering, roughness, ripples, refraction, a wet band, foam, and glint.
- A plane may carry a `Swell` of up to 32 waves. The Low water tier (Desktop Low, Steam Deck, and mobile) draws the first 8, High the first 16, and Ultra the first 32. The frame holds at most 128 waves across every body. Extra waves are counted and not drawn.
- Every animated term is a function of the fixed-step `WaterClock` tick. The same tick draws the same image.
- `WaterBody::surface` and `WaterBody::raycast` answer height, normal, velocity, and crest foam. The agent methods are `water.surface` and `water.raycast`.

The terrain `WaterPlane` becomes one of those plane bodies in the runtime frame
(`look_from_plane`). `WaterSettings::terrain_planes` (`water.set` with `terrain_planes`) turns
that conversion on or off. The plane itself stays in the terrain format.

`docs/specs/87-water-finish.md` adds the shore-distance field, shoaling and shore foam, a
surface query over the bodies, and floating bodies (`water.floaters`).
`docs/specs/water-ocean.md` adds an FFT ocean band a plane can ride.
`docs/specs/100-water-extras.md` adds the underwater view, ring culling, a sea-state table
driven by the scene wind, and a water GPU-time reading. Each of those extras is off until the
project turns it on. A project that sets nothing keeps the frames it drew before.

Not in this version: a lake whose outline is an arbitrary shape. The water GPU time is a second
draw into a private target, not a sample inside the frame's own pass. Swell with a shore field
shoals and caps. It does not turn toward the shore, and it does not run up the beach. Floaters
do not leave wakes. Underwater caustics stay off unless `water.set_caustics` enables them, and
ribbons are not fogged underwater. The recipes and the rest of the limits are on
[Water bodies](../agents/water.md).

## Weather on the ground

<!-- index: Terrain weather -->

Rain, snow, wetness, and the wind near the ground are not fields of the heightfield. They are
the project weather (`docs/specs/99-weather-precipitation.md`), off until
`assets/config/weather.ron` enables them. A layer stack answers that state: format v4 adds a per-layer `weather` block and
the stack's `puddles`, and format v5 adds `porosity_from_orm`. See
[Photoreal terrain](terrain-photoreal.md) and [Sky Lab](sky-lab.md).

## Cook and preview

Terrain is authored as text and images and cooked like other assets; `regia preview` shows a mesh
asset on a neutral stage, and `seeing.screenshot` with `target: "asset"` renders it headless for
goldens. The procedure, including the source-inspect and cook checks, is
[Author a terrain](../how-to/world/terrain.md). Import limits, tile sets, and overview images are
[Terrain source import](terrain-sources.md). A signed-distance cave is
[Volumetric terrain](volume-mesh.md). The integer voxel core in `docs/specs/122-voxel-core.md`
is not in this version.
