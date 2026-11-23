# Per-vertex ambient occlusion

An opaque lab material can read the alpha of its vertex colors as a smooth occlusion factor on its
indirect light. It exists for interiors that fake sky falloff by splitting a mesh into stepped
material variants (each step prints a hard edge): bake the falloff into the vertices instead and the
interpolation across each triangle is the gradient.

## What it does

With the flag on, `LabVertex::color[3]` is not an opacity. The fragment shader clamps it to 0..1
(linear, no curve), interpolates it like any vertex attribute, and multiplies it into:

- the diffuse indirect light (sky irradiance, irradiance probes, baked GI, the ground bounce), and
- the specular indirect light (the reflections that read the same sky and probes).

It does **not** touch the sun, local lights, emissive light or the material's own occlusion texture
and the screen-space AO. A vertex alpha of 1 changes nothing; 0 removes all indirect light there.

With the flag off (the default) the shader reads the vertex alpha as before, and the frame is
bit-identical to one without the feature.

The flag is honored on opaque, lit materials only. A masked, blended, additive, unlit, hair or
transmissive material keeps the vertex alpha as its opacity (`active: false` below).

## Names

| Where | Name |
|---|---|
| Material field | `LabMaterial::vertex_ao: bool` (default `false`) |
| Vertex input | `LabVertex::color[3]`, 0..1 |
| Packed constant | `alpha.w` bit 1 (value 2) of the material block; bit 0 stays the packed-ORM flag |
| CPU reference | `regia_render::vertex_ao::{factor, scale_indirect, is_active, write_vertex_ao, alpha_range}` |
| glTF import | `.meta` setting `vertex_ao: true` (`GltfImporterSettings::vertex_ao`) |
| Agent | `material.vertex_ao`, `material.set_vertex_ao` |

## glTF import

glTF `COLOR_0` already cooks as RGBA. With `(vertex_ao: true)` in the `.meta` settings the importer
clamps the alpha to 0..1 (a non-finite value becomes 1) before it cooks the mesh. The setting does
not touch the cooked material: the game turns the flag on at runtime (the agent method, or
`LabMaterial::vertex_ao = true`). Authored `.material.ron` files have no field for it yet (ADR-1512).
`CpuMesh::from_cooked` still writes white `LabVertex::color`. The forward shader multiplies that
attribute into base color, so the cooked stream stays on the asset. A draw that wants the color
on the vertex copies `cpu.colors` (the editor mesh source does).

## Agent methods

`material.vertex_ao { entity }` returns `{ entity, enabled, active, alpha_min, alpha_max, has_ramp }`.
`alpha_min` / `alpha_max` are the range of the vertex alpha in the mesh (`has_ramp` when they
differ); `active` says the renderer honors the flag for this material.

`material.set_vertex_ao { entity, enabled }` sets the flag and returns `{ entity, before, after }`.
The entity gets its own copy of the material, so entities that shared it keep their setting.

Errors: `F0003` (`-32001`, not found) when the entity has no `MeshRenderer`; `F0002` (`-32602`,
invalid params) when `enabled` is missing or not a boolean.

Captured on lavapipe (llvmpipe Vulkan): the ephemeral cuboid entity had no `MeshRenderer`, so the
handler returned `F0003` before it validated `enabled`.

```json
{"jsonrpc":"2.0","id":4,"method":"material.set_vertex_ao","params":{"entity":4294967094,"enabled":true}}
{"jsonrpc":"2.0","id":4,"error":{"code":-32001,"message":"entity 4294967094 has no MeshRenderer","data":{"code":"F0003","hint":"vertex occlusion goes on a mesh entity: pick one from scene.query_entities"}}}
```

## Checking it

Take `seeing.screenshot` with `enabled` false and true: a surface with an alpha ramp darkens
smoothly where the vertex alpha falls, with no step at triangle or material boundaries, and a sunlit
patch keeps its brightness.
