# Software masked occlusion culling

`regia_render::masked_occlusion::MaskedOcclusionCuller` is a CPU occlusion culler for
low-end and Android devices, for frames where the GPU Hi-Z path (the depth pyramid and the
two-phase prepass) is unavailable or too expensive. You rasterise a few large occluder meshes
— terrain, buildings, hulls, walls — into a tiny masked depth buffer (256×128 pixels by
default), then test candidate draws' bounding boxes against it and skip the ones that are
entirely hidden.

It follows the masked approach of Hasselgren, Andersson & Akenine-Möller, "Masked Software
Occlusion Culling" (HPG 2016): the buffer is split into 8×8-pixel tiles, and each tile keeps
two depth layers (front and back), each a single depth value plus a 64-bit coverage mask. (The
paper keeps one full-coverage reference layer plus one masked working layer and samples pixel
centres; this culler keeps two masked layers and only marks pixels that are entirely covered,
so it never reports a false `Occluded`.) The culler is single-threaded, deterministic, and portable safe Rust (no SIMD
intrinsics), so the same scene gives bit-identical results on every target.

## Per-frame flow

```rust,wrap
# use regia_core::math::{Aabb, Mat4, Vec3};
# struct Occluder { vertices: Vec<Vec3>, indices: Vec<u32>, model: Mat4 }
# struct Candidate { world_bounds: Aabb }
# fn draw(_: Candidate) {}
# let large_occluders_front_to_back: Vec<Occluder> = todo!();
# let candidates: Vec<Candidate> = todo!();
# let view_projection: Mat4 = todo!();
use regia_render::masked_occlusion::{
    Backface, MaskedOcclusionCuller, Visibility,
};

let mut culler = MaskedOcclusionCuller::new(256, 128)?; // once, keep it around

// Each frame:
culler.clear();
for occluder in large_occluders_front_to_back {
    culler.draw_occluder(
        &occluder.vertices,          // &[Vec3], object space
        &occluder.indices,           // &[u32], triangles
        view_projection * occluder.model,
        Backface::Cull,
    )?;
}
for candidate in candidates {
    if culler.test_aabb(candidate.world_bounds, view_projection)? == Visibility::Occluded {
        continue; // entirely behind the occluders: skip the draw
    }
    draw(candidate);
}
```

`draw_occluder` returns a `DrawReport` (submitted / rasterised / back-face culled / clipped /
degenerate / non-finite triangle counts), and `culler.stats()` accumulates the same counters
across calls. `test_rect_depth(rect, ndc_depth)` tests an already-projected screen rectangle
at its nearest NDC depth, for impostors and particles.

## The contract

- **Never a false `Occluded`.** A coverage bit is only set when the union of one
  `draw_occluder` call's triangles covers the whole pixel, layer depths are far bounds, and box tests expand the projected box and use
  its nearest depth. A partially visible box is always reported `Visible` — including boxes
  crossing the near plane or reaching off-screen. Cull with it freely; verify with the
  culler's property test (2000 seeded random boxes against a ray-cast oracle, zero false
  occlusions allowed).
- **It is not frustum culling.** Off-screen and behind-camera boxes report `Visible`. Run
  your frustum test as usual.
- **Depth mode must match your projection.** `MaskedOcclusionCuller::new` defaults to
  `DepthMode::Reversed`, matching the engine's reversed-Z infinite projection (near plane at
  `z = w`). For a standard projection (near at NDC z = 0), use
  `new_with_depth_mode(w, h, DepthMode::Standard)`. Geometry between the eye and the near plane
  is clipped away, as on the GPU.
- **Submit a mesh in one call.** Triangles of one `draw_occluder` call that share an edge (same
  object-space endpoint positions) fuse: the shared edge leaves no crack. Triangles of separate
  calls only combine where each covers whole pixels on its own, so split a mesh across calls
  only when you must.
- **Matrices are full transforms.** `draw_occluder` takes the object-to-clip matrix
  (`view_projection * model`); `test_aabb` takes the `view_projection` part. Both must agree,
  and both error (`OcclusionError::NonFiniteMatrix`) on NaN or infinite matrices instead of
  producing garbage coverage.
- **Winding.** `Backface::Cull` keeps triangles that wind counter-clockwise in NDC and drops
  the rest — use it for closed occluder meshes seen from outside. `Backface::None` rasterises
  both windings, which suits terrain sheets and other single-sided geometry. Cull only when
  the GPU culls the same faces: a mirrored model matrix (negative scale) or a clockwise front
  face flips the winding, so flip the indices or use `Backface::None` there.
- **Draw order.** Any order is correct. Large, near occluders first gives the tightest layers
  and the best culling rate; `culler.covered_pixel_count()` helps sanity-check a scene.

## Sizing

Bigger buffers cull more (finer silhouettes, fewer edge losses) and cost more to rasterise
into. 256×128 suits a handful of building-sized occluders; go to 512×256 only if profiling
says the extra culling pays for itself, and down to 128×64 on the weakest devices. Tile
counts need not be round — any size from 1×1 to 4096×4096 works, edge tiles are partial.
Occluders should be coarse, watertight-ish proxies (a building's hull, not its window
frames): thin geometry covers few whole pixels and occludes little.

## When not to use it

Devices that run the GPU-driven path already get Hi-Z occlusion from the depth pyramid at
full resolution. The masked culler is for the fallbacks: the forward-lite / low-end tiers,
headless tooling, and platforms where reading GPU depth back into a pyramid is not an option.

The renderer runs it for you on Desktop Low (and the ladder's Very Low) and on the Deck
(`RenderProfile::masked_occlusion`, ADR-10072): each main-view frame rasterizes its largest opaque
rigid meshes and drops the draws they hide from the colour and depth passes. See
[Two-phase occlusion: masked occlusion of the draw list](../agents/occlusion.md#masked-occlusion-of-the-draw-list-cpu-low-and-deck)
for what it culls and how to check it.
It also does not help with transparency sorting or shadow culling — it answers exactly one
question: "is this box entirely behind the occluders I drew?"
