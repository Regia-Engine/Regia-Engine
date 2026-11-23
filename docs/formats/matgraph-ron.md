# `.matgraph.ron` (v5), `.matinst.ron` (v2) and `.matfunc.ron` (v4)

Material graphs compile to the forward pass's `material_surface` function (`docs/specs/13-renderer.md`
§ 7, `docs/specs/22-tier2.md` T2.4). Crate: `regia_matgraph`. Agent guide:
`book/agents/material-graph.md`. Examples: `matgraph.example.ron` (the hard-surface hull panel),
`matinst.example.ron`.

## Graph (`.matgraph.ron`, `MATGRAPH_FORMAT_VERSION = 5`)

| Field | Type | Meaning |
|---|---|---|
| `format_version` | u32 | 5. A newer file is rejected before its body is read; older files run the migrator chain (`1 → 2` only moves the version: every v2 field is optional; `2 → 3` only moves the version: v3 only adds the `Wind` node; `3 → 4` only moves the version: v4 only adds the `Noise` node; `4 → 5` only moves the version: v5 only adds the `Code` node). |
| `name` | string | Display name (not part of the generated code). |
| `params` | `[(name, value, category?, tooltip?, range?)]` | Up to 15 parameters, in uniform-slot order. `value` is `Float(x)`, `Vec2((x, y))`, `Vec3(..)`, `Vec4(..)` or `Color((r, g, b))` (linear); its variant is the parameter's type. v5 fold (ADR-8105 (b)), editor-only, ignored by compile: `category` (string, default empty = ungrouped), `tooltip` (string, default empty), `range: Some((min, max, step))` (`step` default 0 = continuous; default `None` = unbounded). The loader refuses a non-finite bound, `min >= max`, a step outside `0..=max - min` and a default outside the range. |
| `textures` | `[(name, path, color_space)]` | Up to 7 textures, in slot order. `color_space`: `Srgb` (colour) or `Linear` (data). `path` is the default image. |
| `switches` | `[(name, default)]`, v2 | Static switches: compile-time booleans. A `Switch(switch, on, off)` node takes `on` or `off`; only that branch is emitted. Each combination is its own permutation (code, pipeline key). |
| `functions` | map name → `Inline(function)` or `File("assets/….matfunc.ron")`, v2 | Material functions (sub-graphs) that `Call` nodes expand in place. A `File` is resolved by the loader and the cook (`MaterialGraph::resolve_function_files`). |
| `nodes` | map name → node | Inputs refer to other nodes by name; names are `[A-Za-z0-9_]`, ≤ 64 chars. Unreachable nodes are allowed and not emitted. |
| `output` | struct | `base_color` (Vec3), `alpha`, `metallic`, `roughness`, `occlusion` (Float), `normal` (Vec3), `emissive` (Vec3): `Some("node")` or absent (contract default: 0.8 grey, 1, 0, 0.5, 1, geometric normal, 0). `normal_space`: `Tangent` (default) or `World`. v2: `world_position_offset` (Vec3, metres): evaluated per vertex in every pass (forward, depth prepass, shadows, motion vectors); eye-dependent nodes (`ViewDirection`, `CameraDistance`, `Fresnel`) and `Parallax` may not feed it; a `Sample` without `lod` reads level 0 there. |
| `shading` | struct, optional | `alpha_mode` (`Opaque`, `Mask(cutoff)`, `Blend`, `Premultiplied`, `Additive`), `unlit`, `clearcoat`, `clearcoat_roughness`, `anisotropy`, `anisotropy_rotation`, `specular_occlusion` (default true), `multi_scatter` (default true), v2 `max_vertex_offset` (metres; culling and shadow bounds grow by it). These drive the shared lighting and render state, not the graph code. |

Unknown fields are errors. Node kinds and their types are listed in the agent guide; the compiler
reports type errors per node. v2 nodes: `TexCoord1` (second UV set; the first when the mesh has
none), `ObjectPosition` (Vec3), `Sin`, `Cos`, `Frac`, `Switch(switch, on, off)`,
`Call(function, args: {input: node}, output: Some("name"))` (`output` may be omitted when the
function has one), and `Input("name")` (inside a function only). v3 node: `Wind` (Vec2), the
scene wind in m/s on world X (`x`) and Z (`y`) that the host writes beside the material clock
(graph parameter slot 0 `zw`; zero when the host sets none). It reads the same value in the
surface and the vertex stage (one value per frame), and a graph that reads it is time-reading
(the renderer writes slot 0 only into graphs that read `Time` or `Wind`).

v4 node (ADR-2318):
`Noise(input: "p", octaves: 4)`: fractal gradient noise, `Float` in 0..1, sampled at a `Float`, `Vec2`
or `Vec3` position (one unit is one noise cell); `octaves` 1–8 (default 1), each at twice the frequency
and half the amplitude. The lattice hash is integer arithmetic, so every device picks the same gradients (the blend is float math).
It may feed `world_position_offset`.

v5 node (ADR-7852):
`Code(inputs: [(name: "a", ty: Float, node: "u"), ...], textures: ["albedo"], output: Float, code: "return a * b + c;")`:
the body of a WGSL function. Its parameters are `inputs` in order (`node` is the wired node, `""` or absent while
unwired; the wired node must have exactly `ty`), then each of `textures` (declared graph textures) as a
`texture_2d<f32>` of its name and the graph's sampler as `material_sampler`; it returns `output`. The compiler emits
it as a uniquely named function (`mg_code_<n>`, `mgv_code_<n>` per vertex). `#include "name.wgsl"` lines (each its own
line; a plain file name) read `shaders/include/<name>` from the project into the compiled copy only; a
`#resolved "name" <lines>` block is what that copy appends and is reserved. naga checks the node before any pipeline
is built; the rules (no entry points, no module-scope variables or bindings, no `override`, balanced braces, input
names not starting with `mg_` or `__`, at most 16 KiB of code, 16 inputs, 8 includes) and the diagnostics are in the
agent guide (`book/agents/material-graph-editor.md`). A node that fails refuses to compile and cook; the editor
previews it magenta. A Code node with texture inputs may not feed `world_position_offset`.

## Function (`.matfunc.ron`, `MATFUNC_FORMAT_VERSION = 4`)

| Field | Type | Meaning |
|---|---|---|
| `format_version` | u32 | 4 (required in a file, optional inline). `1 → 2`, `2 → 3` and `3 → 4` only move the version: the node set is part of this format; v2 adds the `Wind` node with matgraph v3, v3 adds the `Noise` node with matgraph v4, v4 adds the `Code` node with matgraph v5. |
| `name` | string | Display name. |
| `inputs` | `[(name, ty)]` | `ty` is `Float`, `Vec2`, `Vec3` or `Vec4`; each `Call` argument is checked against it. |
| `nodes` | map name → node | `Input("name")` reads an argument. `Param` and texture names refer to the calling graph's declarations; `Call` may use the calling graph's other functions (no recursion, at most 8 deep). |
| `outputs` | map name → node | |

A call expands under the call node's name, so a call node `worn` names an error inside it `worn/out`.

## Instance (`.matinst.ron`, `MATINST_FORMAT_VERSION = 2`)

| Field | Type | Meaning |
|---|---|---|
| `format_version` | u32 | 2 (same version rules; `1 → 2` only moves the version). |
| `parent` | string | The graph (path or library name). |
| `params` | map name → value | Overrides; the variant must match the parent's (`Color` and `Vec3` are interchangeable). |
| `textures` | map name → path | Image overrides for declared textures. |
| `switches` | map name → bool, v2 | Static switch overrides: selects the compiled permutation. |

Parameter and texture overrides never change code, so all instances of a graph with the same
switch values share one compiled function and one pipeline set.

## Cooked form

`regia cook` compiles every `.matgraph.ron` (its switch defaults) and `.matinst.ron` (its
parent's permutation for its switches, with its overrides), validates the code in every host
shader, and writes `regia_matgraph::CookedMaterialGraph` (postcard, `COOKED_MATGRAPH_VERSION =
1`): the generated WGSL, the parameter block, texture bindings and the shading block. Each
texture is also cooked as an RGBA8 sub-asset `<image path>#rgba8`. `.matfunc.ron` files cook into
the graphs that call them; editing one recooks them. A game loads graphs through the asset server
(`MaterialGraphRef` → `GraphMaterials`) without compiling: no graph compile and no WGSL validation
at run time.

## GPU binding contract

The generated function reads `material.graph[i]` (an `array<vec4<f32>, 16>` appended to the material
uniform; slot 0 `x` = time in seconds, `y` = the previous frame's time, `zw` = the scene wind in m/s on world X and Z, slot *n* = parameter *n*) and the seven material textures
(`albedo_tex`, `normal_tex`, `occlusion_tex`, `emissive_tex`, `detail_tex`, `detail_normal`,
`aux_tex`, in slot order) through `tex_samp`. The renderer splices it into `lab_forward.wgsl` under
`MATERIAL_GRAPH` (`regia_render::surface_graph`). A vertex stage, `fn
material_vertex_offset(in: VertexOffsetInput) -> vec3<f32>`, is spliced into `lab_forward.wgsl`,
`lab_shadow.wgsl` and `velocity.wgsl` under `MATERIAL_GRAPH_VERTEX`; a graph that reads
`TexCoord1` compiles with `MATERIAL_GRAPH_UV1` and draws with a second vertex stream (`f32x2`,
buffer slot 1, location 7). Changing this contract is a format change.

## Freeze rules

All three formats are registered in `docs/formats/format-hashes.ron` (`cargo xtask check-formats`)
and in `regia upgrade-project`. A shape change bumps the version and adds a step to
`regia_matgraph::format::builtin_migrations` / `builtin_function_migrations` /
`instance::builtin_instance_migrations`.

## Editor layout sidecar

The material graph editor (`docs/specs/80-matgraph-remainder.md`, ADR-2318) writes
`<graph>.matgraph.layout.ron` next to each graph it saves: `(format_version: 1, positions: { "node":
(x, y) })`, canvas positions keyed by node name. It is editor-only (never cooked, not read at run time),
is not a frozen content format and is not in `format-hashes.ron`, and is safe to delete: nodes without a
position are laid out by depth from the surface outputs. A file with a newer `format_version` is ignored.

## Floating origin and back faces

A graph may declare 15 parameters. One that also reads `WorldPosition`, `ObjectPosition` or `Triplanar` may
read at most 14 of them: the compiler rejects a position read that uses every slot `material.graph[1]` through
`material.graph[15]`, because the renderer needs one unread slot for the floating origin (spec 78 § 12,
ADR-2435, ADR-5005). A parameter the graph declares and never reads leaves its slot free.

`SurfaceInput` ends with `facing: f32` (1 on a front face, -1 on a back face). Generated tangent-space normals
multiply the tangent by it, so a double-sided back face uses the front frame negated. This is the shader
function's argument list, not the cooked payload: the `.matgraph.ron` schema and `COOKED_MATGRAPH_VERSION` stay
as they are. A graph cooked before this change keeps its old surface text until it is recooked.
