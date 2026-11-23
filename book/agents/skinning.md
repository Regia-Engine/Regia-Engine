# Skinning

Skinned meshes are drawn one of two ways. The default **vertex-shader path** skins each vertex
in every pass that draws it (sun shadow cascades, depth prepass, motion vectors, colour), with
four influences and linear blending. The **compute path** skins each instance once per frame in
a compute pass, and every pass draws that result. It also reads up to eight influences per
vertex and can blend with dual quaternions, so a twisted forearm or shoulder keeps its volume.
Nothing moves to the compute path unless you ask for it.

## `render.skinning`

Reports the compute path. No params.

```json
{ "method": "render.skinning" }
```

Returns `compute_all` (every skinned mesh on the compute path), `supported` (`false` on a
device without compute shaders; `unsupported` says why), `last_frame` (`instances` drawn from
the compute path, `dispatched` (the rest held a settled pose and were not skinned again),
`vertices`, `dual_quaternion`, `eight_influences`, `influence_mismatch` (an eight-influence
table whose length is not the mesh's vertex count: drawn with four), `resident`, `options`,
`counted` (instances skinned from a counted mesh's upload: the kernel reads the adopted vertex
buffer, or a GPU copy of it, ADR-7721)),
`gpu_bytes` (what the compute path holds on the GPU: source copies, two output buffers per
instance, palettes and tables; outside the mesh cache budget, freed after about 240 idle frames
or under memory pressure), and `entities` (every entity with options: `method`, `compute`,
`influences8` (table length or `null`), `on_compute_path`). `last_frame` and `gpu_bytes` are
`null` without a runtime renderer.

## `render.set_skinning`

Params: `compute_all` (boolean: every skinned mesh), or `entity` with any of `method`
(`"linear"` or `"dual_quaternion"`), `compute` (linear skinning on the compute path) and
`clear` (remove the entity's options).

```json
{ "method": "render.set_skinning", "params": { "compute_all": true } }
{ "method": "render.set_skinning", "params": { "entity": 4294967301, "method": "dual_quaternion" } }
{ "method": "render.set_skinning", "params": { "entity": 4294967301, "clear": true } }
```

Returns `{ before, after, note }` (both are `render.skinning` results). Changes apply from the
next extracted frame. Options on a character's root cover its skinned children. Eight-influence
tables come from code (`SkinInfluences8::from_cpu_skin`), not from this method.

Check it worked:

1. `render.skinning` after a frame: `last_frame.instances` counts the meshes you moved, and
   `dual_quaternion` counts the entities you set.
2. Twist test: pose a forearm 150° or more about its own axis. Capture a screenshot with
   `"linear"` and one with `"dual_quaternion"` and compare: the linear wrist pinches, the dual
   quaternion one stays round.
3. On a device where `supported` is `false`, everything draws linear with four influences.
   That is the fallback, not a bug.

## Eight influences from the cook (ADR-1622)

A mesh draws eight influences per vertex only when its asset opted in: the glTF's `.meta`
settings say `skin_influences: 8` and the skin has a second set (`JOINTS_1`/`WEIGHTS_1`). The
cook then writes `skin_influences` 8 (cooked mesh v3); everything else is the same bytes as the
default cook. The default is 4 influences, the vertex-shader path's four strongest, exactly as
before. The host attaches the table with `SkinningOptions::for_cooked_skin(&skin)` on the mesh
entity (it returns nothing for a default mesh).

Check it worked:

1. `assets.skin_check` on the cooked mesh: `stored_influences` 8 and `draw_influences` 8 (the
   default cook answers `draw_influences` 4).
2. `render.skinning` after a frame: the entity lists `influences8` (the vertex count) and
   `on_compute_path: true`, and `last_frame.eight_influences` counts it. A table of the wrong
   length shows in `influence_mismatch` and draws with four.
