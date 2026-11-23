# Decals: scorch, pad markings, ruts

Project a prefab onto a hit, lay a strip along a path, or read how many decals the last view kept.
These need `regia_vfx::VfxPlugin` (a `DecalPool`). Without it every call returns `UNSUPPORTED`
(`F0016`) and does not enqueue a spawn.

Prefabs are `.decal.ron` version 3 (v1/v2 migrate) (`docs/formats/decal-ron.md`). The built-in names are `scorch`,
`pad_marking`, `rut` and `grime`. Each one's receiver mask is terrain and static meshes, so a
character layer is left clean when the host supplies a receiver buffer. The frame renderer
writes that buffer: it clears to terrain (layer 0, world up), stamps meshes, then lets native
terrain overwrite uncovered pixels with layer 0 and the heightfield normal
(`SharedPass::Receiver`). A rigid mesh stamps as static (layer 1) and a skinned mesh as a
character (layer 2), unless `receiver_layer` on the frame mesh or `MeshRenderer::decal_layer`
is non-zero — that value is the layer, so a rigid prop can be masked without a joint palette.
The depth those passes attach is `depth_for_output` (the scene depth, or a nearest copy at
the output size on a scaled frame). The LDR color is copied when the target allows
`COPY_SRC`. The stamp, the terrain receiver, the copy and the decal draw share the frame's
one command encoder.
Each survivor is one screen quad over the 16 px tiles it touched.

## Spawn on a hit

`vfx.spawn_decal` places one prefab. `position` is required. `normal` defaults to `[0, 1, 0]`
(a zero vector is rejected). `yaw` is radians. It spins a seed around world +Y and projects that
onto the plane: on a floor the decal turns, and on a vertical wall it does not make a 90° turn
around the normal. In-plane heading is `align_along`. `prefab` defaults to `scorch`.
`lifetime` overrides the prefab; omit it to keep the prefab's own lifetime.

```json
{ "position": [1.0, 0.0, 2.0], "normal": [0, 1, 0], "prefab": "scorch" }
→ { "index": 0, "slot": 0, "generation": 0, "live": 1, "capacity": 32, "prefab": "scorch", "normal": [0, 1, 0], "texture": null, "atlas_layer": 0 }
```

`normal` in the result is the decal's local +Z after alignment, not a copy of the input when the
input was un-normalized. `index` is the decal's current position in the pool's draw list, which
shifts as other decals retire. `slot` + `generation` is its stable id: it stays valid while the
decal lives and goes stale when it retires or the full pool recycles it. An unknown prefab is
`INVALID_PARAMS`.

## Rut strip

`vfx.place_rut` stamps overlapping decals along `points` (at least two). Defaults: prefab `rut`,
spacing `0.45` m, half-width `0.18` m, half-depth `0.15` m. Spacing below `0.05` m is raised to
`0.05` (the error text quotes that raised spacing). `half_width` and `half_depth` below `0.01` m
are raised to `0.01`; they are not rejected. `curve: true` with at least three points follows the
quadratic of the first three (12 samples). Extra points are not stamped, but the length check
sums every supplied segment first, so a far unused tail is `INVALID_PARAMS`. A rut is at most
4096 stamps, and every point must be finite; otherwise the call is `INVALID_PARAMS`.

```json
{ "points": [[-4, 0, 2], [0, 0, -2], [4, 0, 2]], "curve": true }
→ { "placed": 21, "live": 21, "capacity": 32 }
```

`placed` is how many stamps were built, before the pool's capacity drops the oldest. The curve
above is sampled at 13 points and stamped every 0.45 m, which is 21 stamps. `capacity` is the
pool's own size: 32 here is a small pool, and Desktop High reports 256. `placed` stays 21 while
the pool can hold them.

## Budget

`vfx.decal_budget` is read-only.

```json
{}
→ { "capacity": 32, "live": 12, "culled": 0, "tiles_touched": 0 }
```

The reply also carries `shading`: the path that shades each live decal (`post`, `forward` or
`split`) under the forward-decal switch, and the forward pass's tile counts for the view. See
[Forward decals](forward-decals.md) (ADR-3255).

`culled` and `tiles_touched` are computed on the call when a `VfxView` is present. Without one
they stay 0, or the last figures a renderer stored. The `capacity: 32` answers above are a small
pool. Desktop High keeps 256 decals; Desktop Ultra keeps 512. The upload sorts by priority and
keeps the highest when the visible set exceeds the profile budget.

## Layering order

Decals draw lowest `priority` first, so the highest lies on top. Decals of equal priority draw
in spawn order: the newest lies over the older one. A full pool recycles its oldest slot in
place, so `DecalPool::decals()` is in slot order, not spawn order; hosts stage
`DecalPool::in_spawn_order()` instead (the VFX runtime and `vfx.decal_budget` do). When the
visible set exceeds the profile budget the lowest-ranked decals are dropped first, so among
equal priorities the oldest go.

## Static markings

Pad paint and grime that never move are a `StaticDecal` component on a level entity (`prefab`,
`yaw`, optional `receiver_mask`). The plugin copies them into the pool when the component is
added, and once more at startup for entities that already exist. Identity rotation projects
along world +Y.

### More `vfx.place_rut` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `half_width` (number): Half-width across the path. Default 0.18.
- `half_depth` (number): Half-depth of the projection. Default 0.15.

## Decals in the reference trace and in the AOVs (ADR-2951)

The reference path tracer draws the same projected decal: `regia_pt::PtScene::decals` holds one
`regia_pt::PtDecal` per decal, with the fields of `Decal` and `DecalSurface` the decal shader reads. The tracer has
no `regia_vfx` dependency, so a host copies them. Use `PtDecal::from_box(position, rotation, half_size, color)` and
then set `roughness`, `normal_xy`, `normal_strength`, `emissive`, `angle_fade`, `distance_fade`, `priority`,
`receiver_mask`, `channels`, `edge_softness` and `blend`. Fold the lifetime fade into `opacity`. An atlas cell is a
`PtScene::textures` index (`atlas`): sRGB colour with coverage in alpha, sampled clamped like the frame's atlas
sampler.

The tracer uses the frame's projection and weights: the box test, receiver mask, grazing fade, edge feather,
distance fade and atlas coverage. It applies the decal to the hit's material inputs:

- **Albedo**: modulated (`blend` 0) or replaced (`blend` 1), weighted by the decal's weight.
- **Normal**: the atlas luma read as a height along the decal's axes, then the authored tangent-space normal
  blended in by whiteout. `normal_strength` weights both.
- **Roughness**: the decal's roughness, mixed in by the weight.
- **Emission**: added, weighted, as scene radiance. The frame's decal pass adds `DecalSurface::emissive` in exposed
  light, so to match a frame, divide it by that frame's exposure when you copy it into `PtDecal::emissive`.

`pack` leaves out a decal that is not `PtDecal::is_well_formed`: a non-finite number, a zero-length rotation or a
box of zero size. Such a decal would otherwise apply everywhere with a NaN weight and black out the trace.

So the trace lights the decal with the BRDF, and its `normal` AOV and `PtImage::roughness` hold the decal's values.

A scene without decals traces the base kernels, and its image is unchanged bit for bit. A scene with decals
composes the decal code into its own kernel set, cached like the material-graph sets
(`PathTracer::graph_kernel_compiles` counts both). Probe and lightmap bakes do not apply decals.

The frame is different. Its decal pass relights the composited colour after the forward pass, so the frame's
normal and roughness AOV (`seeing.frame_aov`, [Frame AOVs](frame-aov.md)) holds the receiver's values on a decal's
footprint, not the decal's. The spec 27 suite's `decal_aovs` row records this as a fail with the numbers
([Spec 27 calibration](spec27-calibration.md)).

Custom images and the `texture` / `atlas_layer` spawn result fields are covered in
[Custom decal textures](custom-decal-textures.md).
