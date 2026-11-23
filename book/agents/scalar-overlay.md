# Scalar overlay (per-vertex heatmaps)

## `editor.set_scalar_overlay`

Colors one mesh entity by one number per vertex, drawn unlit over the lit viewport: skin-weight
heatmaps, deformation heatmaps, damage brushes. The material and the lighting are untouched.
Params: `entity` (bits of the mesh entity, required: the `entity` of `editor.pick_hit`, not a
model's owner), then at most one value source:

- `values`: one number per vertex, in vertex order;
- `constant`: every vertex gets this number;
- `axis` (`x`, `y`, `z`): each vertex's object-space coordinate on that axis;

and any of `paint` (`{ "vertices": [i, ...], "value": v }`: set those vertices on top of the given or
current values, zeros for a new overlay; an index that is negative, not a whole number (`1.9`), or
at or above the vertex count is skipped and paints nothing), `ramp` (`viridis`,
`diverging`, `grayscale`, or stops `[{ "at": 0..1, "color": [r, g, b] }]` in sRGB 0-1), `range`
(`[lo, hi]`, default `[0, 1]`: the values at the start and end of the ramp, clamped outside; a
reversed range flips the ramp; `lo == hi` is a step), `opacity` (0-1, default 1), `clear` (remove it).
Ramp, range and opacity can change alone on an existing overlay without resending values.

```json
{ "method": "editor.set_scalar_overlay", "params": { "entity": 4294967301, "axis": "y", "range": [-0.5, 0.5] } }
{ "method": "editor.set_scalar_overlay", "params": { "entity": 4294967301, "values": [0, 0.2, 1.0], "ramp": "diverging", "range": [-1, 1], "opacity": 0.8 } }
{ "method": "editor.set_scalar_overlay", "params": { "entity": 4294967301, "paint": { "vertices": [10, 11, 12], "value": 1.0 } } }
{ "method": "editor.set_scalar_overlay", "params": { "entity": 4294967301, "clear": true } }
```

Returns `{ entity, name, overlay: { vertices, values, fit, min, max, ramp, range, opacity } | null,
undoable: false }`. **`fit` must be `"exact"`**: a values count that differs from the vertex count
returns `{ mismatch: "short" | "long", values, vertices }` and the overlay draws nothing (never an
error, never a crash). NaN values read as 0. Infinities saturate at ±1e30 and then go through
the same range division as a finite value, so a range wider than 1e30 can place `+inf` inside the ramp.

Works on rigid, skinned (it follows the joint palette) and instanced meshes (entities sharing one
mesh each get their own overlay or none). It draws after the post stack, so the colors are exactly
the ramp's: no exposure, tonemap or bloom. Surfaces behind nearer geometry stay hidden.

Check it worked: `editor.capture_viewport` before and after; only the targeted entity's pixels
change, and `clear` restores them exactly.

## Rust API (for tools linking the crates)

`regia_render::{ScalarOverlay, Ramp, RampStop, ramp_position}` (component and data),
`VertexAdjacency::new(indices, vertex_count).neighborhood(vertices, &BrushSeed, radius)` and
`apply_brush(values, &neighborhood, radius, BrushFalloff, BrushMode, strength)` for brushes: a pick
hit's `indices` and `barycentric` fill the `BrushSeed`.

Invalid/missing entity bits, non-mesh entities, multiple value sources, or malformed ramps/ranges
return `F0002`. A length mismatch in `values` is a successful response whose `fit` is the object
`{ "mismatch": "short" | "long", "values", "vertices" }` instead of the string `"exact"`, as described above. Workflow: get the mesh entity from
`editor.pick_hit`, set values or an axis ramp, capture the viewport to review, then send `clear: true`
when finished.

Failing call (lavapipe; the entity bits name no entity in the editor world, so the existence check fails
before the mesh check):

```console
$ regia agent call editor.set_scalar_overlay '{"entity":4294967295,"constant":0.5}' --project samples/arena
error: method call failed: no entity 4294967295 (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```
