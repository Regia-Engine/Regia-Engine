# Scalar overlay: per-vertex heatmaps and brushes

A `ScalarOverlay` component on a mesh entity colors the mesh by one `f32` per vertex through a
ramp, drawn **unlit over the lit frame**. It is for tools: skin-weight views, deformation heatmaps
for range-of-motion checks, damage brushes. It is not a material; the mesh keeps its material and
its place in instanced batches.

```rust,wrap
use regia_render::{Ramp, ScalarOverlay};
# use regia_ecs::prelude::World;
# use std::sync::Arc;
# let mut world = World::new();
# let mesh_entity = world.spawn_empty().id();
# let weights: Arc<[f32]> = Arc::from([0.25, 0.5]);

world.entity_mut(mesh_entity).insert(
    ScalarOverlay::new(weights)          // one f32 per vertex, in vertex order
        .with_ramp(Ramp::viridis())      // or Ramp::diverging(), Ramp::grayscale(), Ramp::from_stops(..)
        .with_range(0.0, 1.0)            // values at the ramp's start and end, clamped outside
        .with_opacity(0.85),
);
```

- **Values.** The count must equal the mesh's vertex count. A mismatch draws nothing and is counted
  in `FrameRenderer::scalar_overlay_stats().mismatched` (and logged); it never panics. Editing a
  value means building a new `Arc<[f32]>`; the GPU copy is written once per new `Arc`. NaN reads as
  `0.0`. Infinities saturate at ±1e30 before the range division.
- **Range.** `ramp_position(value, range)` is linear and clamped to 0-1. A reversed range flips the
  ramp, an empty range steps at its value. Use `Ramp::diverging()` with a symmetric range for signed
  quantities (zero is the neutral middle).
- **Ramps are data.** `Ramp::from_stops` takes `RampStop { at, color }` in sRGB 0-1, sorted and
  clamped. The shader samples a 64-entry table baked from the stops.
- **Cost.** Meshes without an overlay cost nothing. An overlaid entity costs one draw of its mesh
  after the post stack. Steady frames and edits create no GPU buffers, bind groups or pipelines
  (see `tests/scalar_overlay.rs`).
- **Skinned and instanced.** Skinned meshes follow their joint palette. Entities sharing one mesh
  `Arc` each carry their own overlay.
- **Brushes.** Build `VertexAdjacency::new(&mesh.indices, mesh.vertices.len())` once, then for each
  stroke `adjacency.neighborhood(&mesh.vertices, &BrushSeed { indices, barycentric }, radius)` (the
  vertices within `radius` along the surface, nearest first) and `apply_brush(..)` for a new values
  `Arc`. The seed fields are the editor `PickHit`'s `indices` and `barycentric`.

Hosts that render an `ExtractedFrame` themselves: see the DO27 host checklist, "Scalar overlay".
