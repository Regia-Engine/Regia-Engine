# Material graphs

A **material graph** (`.matgraph.ron`) describes a surface as a graph of nodes (texture samples,
math, triplanar projection, height blends, Fresnel) instead of fixed parameters. `regia_matgraph`
compiles the graph into the forward pass's `material_surface` WGSL function. The graph decides
only the surface: base color, normal, roughness, metallic, occlusion, emissive and alpha. The
lighting (sun, local lights, image-based light, GI, clearcoat, multi-scatter) stays the engine's
single hand-written code path, so a graph material is lit exactly like a standard one.

Use a standard `.material.ron` ([Materials](../user-guide/materials.md)) for ordinary PBR
surfaces; reach for a graph when a surface needs logic: dust that settles on upward faces, wear on
edges, a blend between rock and snow by height, a pulsing emissive.

## A graph

```ron,matgraph
(
    format_version: 4,
    name: "dusty_metal",
    params: [
        (name: "tint", value: Color((0.55, 0.56, 0.58))),
        (name: "dust", value: Color((0.42, 0.27, 0.16))),
        (name: "tiles", value: Float(0.5)),
    ],
    textures: [(name: "grime", path: "textures/grime.png", color_space: Linear)],
    nodes: {
        "tint": Param("tint"),
        "dust": Param("dust"),
        "tiles": Param("tiles"),
        "grime": Triplanar(texture: "grime", scale: "tiles", sharpness: 4.0),
        "grime_r": Swizzle(input: "grime", channels: "r"),
        "n": WorldNormal,
        "up": Swizzle(input: "n", channels: "y"),
        "settle": Mul("up", "grime_r"),
        "mask": Saturate("settle"),
        "base": Lerp(a: "tint", b: "dust", t: "mask"),
        "metallic": OneMinus("mask"),
        "rough_lo": Float(0.35),
        "rough_hi": Float(0.95),
        "roughness": Lerp(a: "rough_lo", b: "rough_hi", t: "mask"),
    },
    output: (base_color: Some("base"), metallic: Some("metallic"), roughness: Some("roughness")),
)
```

- `params` are the values an artist (or an instance) tunes: `Float`, `Vec2`/`Vec3`/`Vec4` or
  `Color`. They live in a uniform block, so changing one never recompiles anything.
- `textures` declare the images the graph samples, with their color space (`Srgb` for color,
  `Linear` for masks and normals).
- `nodes` is a map from a node name to its definition; inputs refer to other nodes by name.
- `output` wires nodes to the surface slots. `base_color`, `emissive` and `normal` take a `Vec3`, the
  others (`alpha`, `metallic`, `roughness`, `occlusion`) a `Float`; `normal` is tangent space
  unless the `output` block says `normal_space: World`.
- An optional `shading` block sets the alpha mode (`Opaque`, `Mask(cutoff)`, `Blend`,
  `Premultiplied`, `Additive`) and other render state.

## Node kinds

| Group | Nodes |
|---|---|
| Inputs | `TexCoord`, `VertexColor`, `WorldPosition`, `WorldNormal`, `ViewDirection`, `CameraDistance`, `Time`, `Wind` (v3), `Param`, `Float`, `Vec2`, `Vec3`, `Vec4` |
| Textures | `Sample` (optional `lod`), `Triplanar`, `UvTransform`, `Parallax` |
| Math | `Add`, `Sub`, `Mul`, `Div`, `Min`, `Max`, `Pow`, `Dot`, `Lerp`, `Smoothstep`, `Saturate`, `OneMinus`, `Abs`, `Normalize`, `Length`, `Swizzle`, `Append` |
| Noise (v4) | `Noise(input, octaves)`: fractal gradient noise in 0..1 at a `Float`, `Vec2` or `Vec3` position, 1–8 octaves, the same lattice gradients on every device (integer hash; the blend is ordinary float math) |
| Shading | `Fresnel`, `HeightBlend`, `UnpackNormal`, `NormalStrength`, `BlendNormals` (`Rnm`, `Udn`, `Whiteout`) |

The compiler infers every node's type. `Add`, `Sub`, `Mul`, `Div`, `Min`, `Max` and `Pow` accept a
`Float` beside a vector; other widths must match. `Lerp`'s `t` may be a `Float` or the same width as
the edges. `Dot` needs two vectors of one width (a `Float` is an error). Errors name the node they belong to (`"rgb"`), a declaration (`"param:tint"`,
`"texture:albedo"`) or an output slot (`"output:base_color"`).

## Instances

A `.matinst.ron` reuses a graph with different parameter values and textures. Instances share the
parent's compiled code and pipelines, so a hundred rock variants cost one shader:

```ron
(
    format_version: 2,
    parent: "materials/scatter_rock.matgraph.ron",
    params: {
        "tint": Color((0.035, 0.032, 0.03)),
        "roughness": Float(0.95),
    },
)
```

## Using a graph from code

```rust
use std::collections::BTreeMap;

use regia::prelude::*;
use regia_matgraph::{MaterialGraph, compile, default_params, lab_material};
use regia_render::CpuMaterial;

/// Compiles a graph with no textures into a renderer material.
fn graph_material(ron_text: &str) -> Option<CpuMaterial> {
    let graph = match MaterialGraph::from_ron(ron_text) {
        Ok(graph) => graph,
        Err(err) => {
            warn!("graph does not parse: {err}");
            return None;
        }
    };
    let compiled = match compile(&graph) {
        Ok(compiled) => compiled,
        Err(err) => {
            warn!("graph does not compile: {err}");
            return None;
        }
    };
    let params = default_params(&graph, 0.0);
    lab_material(&graph, &compiled, params, &BTreeMap::new()).ok()
}
```

`compile` type-checks the graph and validates the generated WGSL by composing it into the forward,
depth, shadow and velocity shaders; `compile_unvalidated` skips the shader validation (the book's own checks use it).
`lab_material` returns a `CpuMaterial` (the renderer's material type) whose surface stage is the
graph: put it in a `MeshRenderer` like any other material. For textured graphs, pass a map from
each declared texture name to its `LabTexture`. `instance_material` does the same for an
instance, merging its overrides over the parent's defaults.

Compiles are cached by the graph's **structure and switch values** (`permutation_hash`; `graph_hash` is the declared-default permutation): two graphs with the same nodes
and wiring but different parameter values share one compile, and the renderer keys its pipeline
sets by the generated code, so parameter edits and texture swaps never trigger a shader
build (a static-switch override is another permutation).

## Switches, functions and vertex offset

Format v2 (v1 files load unchanged and `regia upgrade-project` rewrites them) adds:

- **Static switches.** `switches: [(name: "weathered", default: true)]` declares a compile-time
  boolean and a `Switch(switch: "weathered", on: "worn", off: "tint")` node picks one input. An
  instance that sets `switches: { "weathered": false }` gets its own shader permutation; the
  unused branch costs nothing at run time.
- **Functions.** A named sub-graph with typed `inputs` and `outputs`, either inline in the graph's
  `functions` map or shared in a `.matfunc.ron` file, used through a `Call(function: ..., args:
  {...})` node. Calls expand in place, so errors inside one name the call and the inner node.
- **`world_position_offset`.** A `Vec3` output that moves vertices (wind sway, a pulsing
  membrane). The offset applies in every pass, so the surface, its depth, its shadow and its motion
  vectors stay together; set `shading.max_vertex_offset` so culling keeps the displaced mesh.
- **`TexCoord1`.** The mesh's second UV set (the first set when the mesh has none).

A complete v2 example is in the [agent guide](../agents/material-graph.md).

Format v3 (v2 files load unchanged and `regia upgrade-project` rewrites them) adds the **`Wind`**
node: the scene wind as a `Vec2` in meters per second on world X and Z, the value the renderer
writes beside the material clock. It reads the same in the surface and the vertex stage, so a
`world_position_offset` can bend a plant along the wind; a graph that reads it is time-reading.
An example is in the [agent guide](../agents/material-graph.md#wind-v3).

Format v4 (v3 files load unchanged and `regia upgrade-project` rewrites them to v4; `.matfunc.ron`
goes to v3 in the same step) adds the **`Noise`** node above. A call expands under the call node's
name, so an error inside a call node `worn` is `worn/out`.

## In a game

Put a `MaterialGraphRef { path }` component next to a `MeshRenderer` and add `MaterialGraphPlugin`:
the entity draws with the cooked graph or instance at `path`. In a scene file:

```ron
"regia_matgraph::runtime::MaterialGraphRef": (path: "assets/materials/hull.matinst.ron"),
```

Cooked graphs load without a compile, and a recook hot-reloads every entity that uses the asset.
New graphs are prewarmed on the renderer's worker threads during a level load, so the first frame
builds no graph pipeline on the frame thread; a graph first met mid-level draws with the standard
surface until its pipelines are ready. `FrameRenderer::material_graph_stats` reports both
(`built_on_frame_path`, `fallback_draws`). Graph draws share the GPU-driven instanced path with
standard materials.

## Editing with an agent

`MaterialGraphPlugin` puts a `MaterialGraphLibrary` resource in the world and enables three agent
methods: `material.graph_get` (list graphs, or read one as RON, JSON and a flat node list),
`material.graph_set` (create or replace a graph, or upsert individual nodes and outputs; every
call compiles and returns per-node errors), and `material.compile` (the parameter and texture
slot layout, the pipeline key, cache hits and optionally the generated WGSL). Details and examples:
[agent guide: material graphs](../agents/material-graph.md).

## In the editor

The **Material Graph** panel (ADR-2318) shows the open graph as a node canvas: one box per node with
a pin per input, and a **Surface** box with the surface outputs. Right-click the canvas to add a
node; drag from an output pin to an input pin to wire it; right-click a node to drive an output,
duplicate or delete it. A node with a compile error has a red border and its messages under the
title; an unwired input pin is red. The side column shows the live preview sphere (rendered when
the graph compiles), the selected node as editable RON with **Rename** and **Apply**, the parameter,
texture, switch and shading blocks, and the error list (click an error to select its node). Edits
are undoable with the editor's Undo, together with level edits. **Save** writes the graph and an
editor-only `<graph>.matgraph.layout.ron` with the canvas positions. Agents drive the same panel
through `editor.matgraph_*` ([agent guide: material graph editor](../agents/material-graph-editor.md)).

## Limits

- At most 15 parameters (slot 0 of the block is `Time`), 7 textures and 8 nested function calls
  per graph; two UV sets.
- The vertex offset does not bend normals. Decal receivers and the editor's material preview draw
  a vertex-offset mesh at its rest position.
- The editor's **Material Graph** panel edits the top-level graph as a node canvas. Material
  functions and instances open as their own canvas tabs (`editor.matgraph_function_open`,
  `editor.matgraph_instance_open`); see [the material graph editor](../agents/material-graph-editor.md).

## Foreground holograms and UI in world space

An additive graph is a transparent world-space surface. Use `Additive` shading
(`LabMaterial::alpha_mode = ALPHA_ADDITIVE` for a hand-written `SurfaceGraph`),
return its light through the graph's emissive output, and choose emission and
camera exposure together: emission is scene-referred radiance. Keep ordinary
depth testing so nearer opaque geometry can hide it. Additive draws do not write
depth. Use front-facing geometry, or `double_sided` when both sides should show,
and place it beyond the camera's near plane.

An opaque backing is optional. If the design calls for a dark plate, give that
separate mesh an opaque material and put it behind the emitting mesh, with a
real depth separation. A backing in front correctly occludes the holo; coplanar
surfaces are not a reliable layering contract. Its projected coverage determines
how much scenery it hides. It is not required to keep a hologram visible over sky.
If a host uses separate views or render targets, the backing must write the same
view's depth attachment to participate in that holo's depth tests.

The renderer draws opaque geometry, then the sky background, then transparent
meshes, including graph materials (ADR-7963). This applies to the physical Earth
sky and the airless black sky. Aerial perspective, clouds, fog, exposure, and
post-processing still apply at their normal stages. Before this repair, the
physical sky's late replacement could erase an additive surface wherever no
opaque mesh had written depth; a fully covering backing could mask that defect.
The scene regression is `regia_render/tests/holo_over_sky.rs`.
