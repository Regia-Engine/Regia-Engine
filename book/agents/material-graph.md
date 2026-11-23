# Material graphs

A material graph (`.matgraph.ron`, format v4, `docs/formats/matgraph-ron.md`) is a node graph that
compiles to the forward pass's `material_surface` function. The graph decides the surface (base
color, normal, roughness, metallic, occlusion, emissive, alpha); the lighting (sun, local lights,
IBL, GI, clearcoat, multi-scatter) is the engine's one hand-written code path, so a graph material
lights exactly like a standard one. Instances (`.matinst.ron`) override a graph's parameters and
textures without new shader code: instances that override only parameters and textures share the
graph's pipelines (a static-switch override selects another permutation).

## Read and list

`material.graph_get` with no `name` lists the library (`{ graphs: [{ name, revision, nodes, params,
textures }], cache }`). With a `name` it returns the graph three ways: `ron` (the file text),
`graph` (JSON, the shape `material.graph_set` accepts) and a flat `nodes` list
(`{ name, kind, inputs, texture }`) that is the quickest way to see the wiring.

## Create and edit

`material.graph_set { name, ron }` (or `graph` as JSON) creates or replaces a graph. To edit in
place, send only what changes:

```json
{ "name": "paint",
  "nodes": { "vcol": "VertexColor",
             "rgb":  { "Swizzle": { "input": "vcol", "channels": "rgb" } },
             "base": { "Mul": ["tint", "rgb"] },
             "old_node": null },
  "output": { "base_color": "base" } }
```

`nodes` upserts by name (`null` deletes), `output` sets slots (`null` clears one); `params`,
`textures` and `shading` replace those blocks whole. Every call compiles and answers
`{ revision, ok, key, errors: [{ node, message }] }`; a graph that does not compile is still stored,
so you can fix a node error with the next edit. `ron` is migrated to format v4 before it is stored.
`graph` JSON is not: `format_version` must already be 4, or the compile error is
`format_version N (migrate to v4 first)` and a later `nodes` edit leaves that version in place.
Errors name the node (`"rgb"`), a declaration
(`"param:tint"`, `"texture:albedo"`) or an output (`"output:base_color"`).

## Compile

`material.compile { name, include_wgsl }` returns the layout the renderer binds:
`params: [{ name, slot, type }]` (uniform `vec4` slots 1–15; slot 0 is time),
`textures: [{ name, slot, binding, color_space }]` (seven slots), `node_types`, the pipeline `key`,
`graph_hash`, `cache_hit` and, with `include_wgsl: true`, the generated WGSL. Graphs with the same
structure and switch values share one compile (`graph_hash`, the hash of that permutation) and one
pipeline set (`key`); parameter values and texture paths never trigger a recompile. A switch
override is another permutation with its own `graph_hash`.

It also returns the expansion's size (ADR-4901): `lowered_nodes` (flat nodes and aliases after the
switches and functions resolve) and `function_calls` (every nesting level counted), with
`limits: { max_lowered_nodes, max_static_switches }`.

Every entry of `errors` (from `material.compile` and `material.graph_set`) is
`{ node, message, code, authored_node }`. `code` is one of `node`, `param`, `texture`, `switch`,
`function`, `output`, `graph` and `generated_code_invalid`. `authored_node` is the node of the graph
the error belongs to: for a node inside an expanded function (`node: "big/half"`) it is the call
node `big`. Only `code: "node"` has one. `generated_code_invalid` is the one failure no authored
node maps to: generated code a host shader rejects, an engine bug. The compile fails on it and the
cook fails with it, so it is never dropped.

```text
→ material.compile { "name": "f" }
← { "ok": false, "errors": [{ "node": "big/half", "message": "unknown node `missing`",
     "code": "node", "authored_node": "big" }], ... }
```

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

Node kinds: inputs (`TexCoord`, `VertexColor`, `WorldPosition`, `WorldNormal`, `ViewDirection`,
`CameraDistance`, `Time`, `Wind`, `Param`, `Float`/`Vec2`/`Vec3`/`Vec4`), textures (`Sample` with an optional
`lod`, `Triplanar`, `UvTransform`, `Parallax`), math (`Add`, `Sub`, `Mul`, `Div`, `Min`, `Max`, `Pow`,
`Dot`, `Lerp`, `Smoothstep`, `Saturate`, `OneMinus`, `Abs`, `Normalize`, `Length`, `Swizzle`,
`Append`), noise (`Noise(input: "uv", octaves: 4)`, v4: fractal gradient noise in 0..1 at a `Float`, `Vec2`
or `Vec3` position, 1–8 octaves) and shading (`Fresnel`, `HeightBlend`, `UnpackNormal`, `NormalStrength`, `BlendNormals` with
`Rnm`, `Udn` or `Whiteout`). `Add`, `Sub`, `Mul`, `Div`, `Min`, `Max` and `Pow` accept a `Float` beside a
vector; other widths must match. `Lerp`'s `t` may be a `Float` or the same width as the edges. `Dot` needs
two vectors of one width (a `Float` is an error).
Surface outputs are typed: `base_color`, `emissive` and `normal` take `Vec3` (use `Swizzle` on a sample),
the rest (`alpha`, `metallic`, `roughness`, `occlusion`) `Float`; `normal` is tangent space unless
the `output` block sets `normal_space: World` (it is not a `shading` field).

`WorldPosition`, `Triplanar` (which projects the world position) and `ObjectPosition` are world positions, also
in a streamed world whose floating origin has moved (spec 78 § 12, ADR-2435): a triplanar rock, world noise or a
wind phase stays on the same world point across a rebase. The compiler's output does not add the origin itself; the
renderer adds it when it composes the graph, from the highest parameter slot the graph does not read
(`regia_render::surface_graph::graph_origin_slot`). A graph that reads a world or object position and also reads
all 15 parameter slots does not compile (`graph`): at most 14 parameters may be read together with a position, so
one slot stays free for the origin. A graph that reads no position may still declare 15 parameters. Far from the
world's zero (beyond about 16 km) these positions have under a millimeter of `f32` precision, as they had before a
rebase. A double-sided back face negates the tangent frame (`SurfaceInput.facing` is 1 on a front face and -1 on
a back face), the same rule as the hand-written normal map.

## Switches, functions, vertex offset (v2)

This sample is a v2 file. `MaterialGraph::from_ron` and `regia upgrade-project --write` migrate it to v4
(the step only moves the version).

```ron,matgraph
(
    format_version: 2,
    name: "swaying_panel",
    params: [
        (name: "tint", value: Color((0.6, 0.62, 0.65))),
        (name: "sway", value: Vec3((0.0, 0.0, 0.08))),
    ],
    switches: [(name: "weathered", default: true)],
    functions: {
        "weather": Inline((
            inputs: [(name: "c", ty: Vec3)],
            nodes: { "c": Input("c"), "rust": Vec3((0.45, 0.22, 0.1)), "k": Float(0.35),
                     "out": Lerp(a: "c", b: "rust", t: "k") },
            outputs: { "out": "out" },
        )),
    },
    nodes: {
        "tint": Param("tint"),
        "worn": Call(function: "weather", args: { "c": "tint" }),
        "base": Switch(switch: "weathered", on: "worn", off: "tint"),
        "t": Time,
        "obj": ObjectPosition,
        "phase_x": Swizzle(input: "obj", channels: "x"),
        "phase": Add("t", "phase_x"),
        "wave": Sin("phase"),
        "uv": TexCoord,
        "height": Swizzle(input: "uv", channels: "y"),
        "amount": Mul("wave", "height"),
        "sway": Param("sway"),
        "offset": Mul("sway", "amount"),
    },
    output: (base_color: Some("base"), world_position_offset: Some("offset")),
    shading: (max_vertex_offset: 0.1),
)
```

- **Static switches** pick a branch at compile time: an instance sets `switches: { "weathered":
  false }` and gets its own permutation (`material.compile` reports the `key`).
- **Functions** expand in place under the call node's name (a call node `worn` names an error inside
  it `worn/out`); they can live
  in a `.matfunc.ron` file (`functions: { "weather": File("assets/materials/weather.matfunc.ron") }`).
- **`world_position_offset`** moves vertices in every pass: the surface, its depth, its shadow and
  its motion vectors stay together. No eye-dependent nodes or `Parallax` may feed it. Set
  `shading.max_vertex_offset` so culling keeps the displaced mesh.
- **`TexCoord1`** reads the mesh's second UV set (`FrameMesh::uv1`, the `MeshUv1` component), or
  the first set when the mesh has none.

## Wind (v3)

`Wind` is the scene wind as a `Vec2`: meters per second on world X (`x`) and world Z (`y`). The
renderer writes it beside the material clock (graph parameter slot 0 `zw`, set by
`FrameRenderer::set_foliage_wind`; zero when nothing sets it), and it reads the same in the
surface and the vertex stage. A graph that reads `Wind` is time-reading (`material.compile`
answers `uses_time: true`), which is what makes the renderer write the slot. This plant bends
along the wind, more toward its top, with a gust that varies per instance:

```ron,matgraph
(
    format_version: 4,
    name: "reed_sway",
    nodes: {
        "wind": Wind,
        "t": Time,
        "op": ObjectPosition,
        "wp": WorldPosition,
        "opx": Swizzle(input: "op", channels: "x"),
        "freq": Float(3.1415927),
        "tw": Mul("t", "freq"),
        "arg": Add("tw", "opx"),
        "s": Sin("arg"),
        "gust_amp": Float(0.2),
        "gust_var": Mul("s", "gust_amp"),
        "gust_base": Float(0.8),
        "gust": Add("gust_base", "gust_var"),
        "wpy": Swizzle(input: "wp", channels: "y"),
        "opy": Swizzle(input: "op", channels: "y"),
        "rel": Sub("wpy", "opy"),
        "inv_height": Float(0.5),
        "h_lin": Mul("rel", "inv_height"),
        "h": Saturate("h_lin"),
        "h2": Mul("h", "h"),
        "wx": Swizzle(input: "wind", channels: "x"),
        "wz": Swizzle(input: "wind", channels: "y"),
        "zero": Float(0.0),
        "wxy": Append("wx", "zero"),
        "w3": Append("wxy", "wz"),
        "stiffness": Float(0.02),
        "bend": Mul("w3", "stiffness"),
        "gusty": Mul("bend", "gust"),
        "offset": Mul("gusty", "h2"),
        "leaf": Vec3((0.18, 0.32, 0.08)),
    },
    output: (base_color: Some("leaf"), world_position_offset: Some("offset")),
    shading: (max_vertex_offset: 0.8),
)
```

The offset is `wind · 0.02 · gust · h²` meters, with `h` the height above the object's origin over
2 m: at the renderer's 40 m/s wind limit and a full gust that is 0.8 m, so
`shading.max_vertex_offset` is 0.8. The gust's 2 s period divides the clock's lap, so it loops
without a seam ([Material clock and wind](material-clock.md)). A surface output can read the wind
too, for example `Length("wind")` to tint leaves in a storm. `Wind` is the v3 node and `Noise` is the
v4 node. v2 and v3 graphs load with the same nodes at v4, and
`regia upgrade-project --write` rewrites `.matgraph.ron` files at v4 and `.matfunc.ron` files at
v3 (the function format holds the same node set).

## In a game

A `MaterialGraphRef { path }` component on an entity with a `MeshRenderer` draws it with the
cooked graph or instance at `path` (`MaterialGraphPlugin`; scene files:
`"regia_matgraph::runtime::MaterialGraphRef": (path: "assets/materials/hull.matinst.ron")`).
Cooked graphs load without a compile; a recook plus the dev cooked watcher hot-reloads every
entity using the asset. New graphs are prewarmed on the renderer's worker threads and a level
load waits for them, so the first frame of a level draws every graph with no pipeline built on
the frame thread (`FrameRenderer::material_graph_stats`: `built_on_frame_path` stays 0). A graph
met for the first time mid-level draws with the standard surface until its pipelines land
(`fallback_draws`). Graph draws use the GPU-driven instanced path like other materials
(`FrameRenderer::draw_stats`: `graph_instanced`).

## Check the result on screen

The renderer draws a graph material through the same frame as everything else, so the
`seeing.*` tools apply unchanged. The parity test renders two hand-written materials and their
graph rebuilds on lavapipe and requires every channel within 1/255 (they match exactly):
`cargo test -p regia_matgraph --test parity`. The runtime paths (instancing, prewarm, vertex
offset in every pass, the second UV set) are `cargo test -p regia_matgraph --test
runtime_paths`; the cook and hot reload are `cargo test -p regia_asset_pipeline --test
matgraph_cook`. Frames: `docs/evidence/material-graph/`, `docs/evidence/matgraph-runtime/`.

## Limits

At most 15 parameters (14 when the graph also reads a world or object position, so the floating origin keeps a
slot), 7 textures and 8 nested function calls per graph; two UV sets. At most
16 static switches per graph (`MAX_STATIC_SWITCHES`: up to 65,536 code permutations, of which the
cook compiles only the ones the graph and its instances pick), and at most 4096 flat nodes after the
functions expand (`MAX_LOWERED_NODES`). A function called many times, nested a few levels, grows
exponentially, so the expansion stops at the bound with one error on the authored call node
(ADR-4901). The vertex
offset does not bend normals. Decal receivers and the editor's material preview draw a
vertex-offset mesh at its rest position.

## Checking foreground emission across the sky

For an additive hologram, compare screenshots of the same fixed camera and
exposure with the emitting mesh visible and hidden. Keep any opaque backing
present in both frames. Check regions on both sides of the terrain/sky silhouette;
the holo must change pixels on each side. Repeat with an airless black sky and
with the backing hidden. A foreground opaque occluder must still hide the holo.
Use the existing material graph and scene-edit methods; no new protocol method
is needed (ADR-7963). The independent lavapipe regression is
`cargo test -p regia_render --test holo_over_sky -- --nocapture`.

`render.graph` continues to expose the existing `forward` and `atmosphere` steps.
The forward step includes the physical sky background before transparency;
`atmosphere` retains aerial perspective and its weather veil. Accordingly,
`forward_ms` includes the background and `atmosphere_ms` measures the remaining
late atmosphere work. See the [world-space holo contract](../systems/material-graph.md#foreground-holograms-and-ui-in-world-space).
