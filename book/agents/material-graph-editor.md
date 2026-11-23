# Material graph editor

The editor's **Material Graph** panel edits a `.matgraph.ron` as a node canvas: one box per node
with a pin per input slot, a **Surface** box with the eight surface outputs, the live preview
sphere, and every compile error on the node it belongs to. The `editor.matgraph_*` methods edit
the same document as the panel, and every edit is on the editor's undo stack (`editor.undo` reverts
it). The graph format and its node kinds are on the [material graphs](material-graph.md) page.

Spec: `docs/specs/80-matgraph-remainder.md` (ADR-2318; functions, instances and tabs: ADR-2490).

## Open or start a graph

```json
{ "method": "editor.matgraph_open", "params": { "path": "assets/materials/paint.matgraph.ron" } }
{ "method": "editor.matgraph_open", "params": { "create": true, "path": "assets/materials/new.matgraph.ron" } }
```

Paths are project-relative (`assets/…`) or absolute, under the project, the temp directory or an `output_roots` entry; a path with `..` or one that leads out is `path_outside_project`
(for `editor.matgraph_open` and `editor.matgraph_save`); the path must end in `.matgraph.ron`. Texture
and function-file paths inside a graph are project-relative: an absolute path or `..` there is a
refused with a message naming the path: the preview and function-file resolution read nothing outside
the project.
The answer is
`{ path, nodes, ok, revision, discarded_unsaved }`. An older graph version is migrated on open (it is
saved at the current version). Opening replaces the document at once: `discarded_unsaved: true` says
edits were dropped, so check `dirty` and save first. Undo steps of the previous graph stay on the
editor's stack but no longer do anything; they cannot change the graph that is open now.

## Read the graph

`editor.matgraph_state { ron? }` answers:

- `nodes`: `{ name, kind, pos: [x, y], inputs: [{ slot, from }], texture, drives, errors, ron }`.
  `from` is `null` for an unwired slot; `drives` lists the surface outputs the node feeds.
- `outputs`: `{ base_color, alpha, metallic, roughness, normal, occlusion, emissive,
  world_position_offset }`, each a node name or `null`.
- `ok`, `errors: [{ target, source, message }]`, `warnings`, `key` and `graph_hash` (hex, when it
  compiles), `permutations` (2^switches).
- `params`, `textures`, `switches`, `functions` (names), `shading`, `dirty`, `revision`,
  `next_undo`, and the file text with `ron: true`.

An error's `target` says where the canvas shows it: `node:<name>`, `param:<name>`,
`texture:<name>`, `switch:<name>`, `function:<name>`, `output:<slot>` or `graph`. An error inside a
material function's expansion (`source: "worn/out"`) is shown on the `Call` node (`node:worn`).
An input that is not wired reads `an input is not connected`.

## Edit

`editor.matgraph_edit { op, … }` makes one undoable edit and answers
`{ op, revision, ok, errors }` (plus `name` for `add`, `readers` for `remove`). A graph that does
not compile is still a valid document: fix it with the next edit.

| `op` | Parameters | Does |
|---|---|---|
| `add` | `node` or `kind`, `name?`, `pos?` | Adds a node. `kind` gives a node with every input unwired; without `name` a fresh one (`mul_1`) is picked |
| `set` | `name`, `node` | Replaces a node (any kind) |
| `remove` | `name` | Removes it; inputs that read it become unwired, outputs it drove are cleared |
| `rename` | `name`, `to` | Renames; every input and output follows |
| `connect` | `from`, `to`, `slot` | Wires input `slot` of `to` to `from` |
| `disconnect` | `to`, `slot` | Unwires a slot |
| `output` | `slot`, `from` (required; `null` clears) | Drives a surface output |
| `move` | `name`, `pos` | Moves a node on the canvas (each call is one undo step; a canvas drag is one) |
| `params`, `textures`, `switches`, `shading` | the block | Replaces that declaration block |

A `node` is RON text (`"Lerp(a: \"x\", b: \"y\", t: \"k\")"`) or the JSON form
(`{ "Lerp": { "a": "x", "b": "y", "t": "k" } }`, `"TexCoord"`). Slot names are the field names of
the kind (`a`, `b`, `t`, `uv`, `lod`, `x`, `input`, `power`, …); one-input math nodes use `x`, two-input
ones `a` and `b`; a `Call`'s slots are its function's inputs. Node names are 1–64 characters of
`A-Z a-z 0-9 _`.

Build a tinted vertex-color material:

```json
{ "method": "editor.matgraph_edit", "params": { "op": "params", "params": [{ "name": "tint", "value": { "Color": [0.8, 0.2, 0.1] } }] } }
{ "method": "editor.matgraph_edit", "params": { "op": "add", "name": "tint", "node": "Param(\"tint\")" } }
{ "method": "editor.matgraph_edit", "params": { "op": "add", "name": "vcol", "kind": "VertexColor" } }
{ "method": "editor.matgraph_edit", "params": { "op": "add", "name": "rgb", "node": { "Swizzle": { "input": "vcol", "channels": "rgb" } } } }
{ "method": "editor.matgraph_edit", "params": { "op": "add", "name": "base", "kind": "Mul" } }
{ "method": "editor.matgraph_edit", "params": { "op": "connect", "from": "tint", "to": "base", "slot": "a" } }
{ "method": "editor.matgraph_edit", "params": { "op": "connect", "from": "rgb", "to": "base", "slot": "b" } }
{ "method": "editor.matgraph_edit", "params": { "op": "output", "slot": "base_color", "from": "base" } }
```

## Compile, preview, save

- `editor.matgraph_compile { include_wgsl?, switches? }` compiles with full validation (the code is
  composed into the forward shader) and answers `{ ok, key, graph_hash, switches, params: [{ name,
  slot, type }], textures: [{ name, slot, color_space }], uses_time, uses_uv1, vertex_offset,
  permutations, policy, errors, wgsl? }`. `switches` picks one permutation (`{ "weathered": false }`);
  `policy` is described under [Permutations](#permutations).
- `editor.matgraph_preview { width?, height? }` renders the preview sphere (64–1024 px, default
  256) and answers `{ width, height, png, mean_rgb, center_rgb, coverage }`: `png` is base64,
  `mean_rgb` the sphere's mean color (0..1), `coverage` its share of the image. Parameters are
  at their defaults and time is 0.
- `editor.matgraph_save { path? }` writes the graph and its layout sidecar atomically and answers
  `{ path, layout, ok }`. A graph with errors is saved too (`ok: false`).

The layout sidecar `<graph>.matgraph.layout.ron` holds canvas positions only; it is editor-only,
never cooked, and safe to delete (nodes without a position are laid out by depth from the outputs).

## Errors

| Code | When |
|---|---|
| `F0002` | Bad parameters: a missing or mistyped field, an unknown `op` or `kind`, a node that does not parse |
| `matgraph_edit_refused` | The document refuses the edit (unknown node, taken or bad name, unknown slot, a node reading itself), or the file to open does not parse |
| `matgraph_preview_failed` | The preview cannot draw a graph with errors; `editor.matgraph_state` lists them. In a function tab: its calling graph cannot be read, or the function has no wired output to show |
| `matgraph_save_failed` | A save failed; the message names the path |
| `F0003` | No graph is open, or the file to open does not exist |
| `path_outside_project` | `path` (open or save) leads outside the project and the allowed roots; nothing is read or written |
| `F0011` | No GPU for the preview |

## Permutations

A graph adds no shader feature axis: each static-switch combination is one permutation of the
surface function. `permutations` is `2^switches`; the state and the panel warn above four switches,
the point where one graph takes a large share of the forward pipeline budget.

`editor.matgraph_compile` also applies the engine's shader-permutation policy (the one the cook and
`regia package`'s ship checks use, `render.permutations`) to the open graph alone. It compiles every switch
permutation (at most 64), and for each real-time render profile of a desktop or Android package
lists what the cook would count:

```json
{ "method": "editor.matgraph_compile", "params": {} }
```

```json
"policy": {
  "permutations": 4, "enumerated": 4, "distinct_codes": 2,
  "profiles": [
    { "profile": "DesktopLow", "sets": 2, "pipelines": 12, "modules": 2,
      "estimated_compile_ms": 205, "budget_pipelines": 4096, "budget_compile_ms": 120000,
      "over_budget": false }
  ]
}
```

(numbers illustrative). `distinct_codes` counts the permutations whose code differs (a switch no
`Switch` node reads changes nothing). `sets`, `pipelines`, `modules` and `estimated_compile_ms`
are this graph's share of the profile's list; the budget is the profile's default (a project
overrides it in `Regia.toml`). `policy` is `null` when a permutation does not compile. In the panel, the
**Permutation policy** section of the side column shows the same count (press **Count**), one cook-report
line per profile.

## Tabs

The editor holds several documents at once, one tab each: graphs (`.matgraph.ron`), material functions
(`.matfunc.ron`) and instances (`.matinst.ron`). `editor.matgraph_open` replaces the active tab;
`editor.matgraph_function_open` and `editor.matgraph_instance_open` open a new tab and keep the others (with
their unsaved edits). Every other method works on the active tab; `editor.matgraph_save` saves it in its own
format.

```json
{ "method": "editor.matgraph_tabs", "params": {} }
{ "method": "editor.matgraph_tabs", "params": { "select": 0 } }
{ "method": "editor.matgraph_tabs", "params": { "close": true } }
```

answers `{ tabs: [{ kind, path, label, dirty, active }], active }` (`kind` is `graph`, `function` or
`instance`). Undo follows its tab: `editor.undo` of a step made in another tab changes that tab, not the
active one; a step of a closed tab does nothing. `editor.matgraph_state` answers `kind` and, for a function,
`function: { name, inputs, outputs }` and `context`; for an instance, `instance: { parent, parent_path,
params, textures, switches }`.

## Material functions

A function opens on its own canvas: its `Input` nodes, its nodes, and an **Outputs** sink with its named
outputs. The same node edits work (`editor.matgraph_edit`); `output { slot, from }` wires a named output.
A function has no declarations of its own: its `Param`, texture and `Switch` nodes name those of the graph
that calls it, so it is checked against a calling graph, the `context` (default: the graph tab it is opened
from). Errors map to the function's nodes (`node:<name>`) and outputs (`output:<name>`).

```json
{ "method": "editor.matgraph_open", "params": { "path": "assets/materials/hull.matgraph.ron" } }
{ "method": "editor.matgraph_function_open", "params": { "path": "assets/materials/wear.matfunc.ron" } }
{ "method": "editor.matgraph_function_signature", "params": {
    "inputs": [{ "name": "tint_in", "ty": "Vec3" }, { "name": "amount", "ty": "Float" }],
    "outputs": ["result"] } }
{ "method": "editor.matgraph_edit", "params": { "op": "set", "name": "color", "node": "Input(\"tint_in\")" } }
{ "method": "editor.matgraph_save", "params": {} }
{ "method": "editor.matgraph_function_users", "params": {} }
```

- `editor.matgraph_function_open { path, create?, context? }` answers `{ path, tab, nodes, ok, inputs,
  outputs, context, revision }`. `create: true` starts a function with input `x` (`Vec3`) and output `out`.
- `editor.matgraph_function_signature { inputs, outputs }` replaces the signature in one undo step. Outputs
  that stay keep their wire; a new input gets an `Input` node of its name when that name is free. An
  `Input` node naming an input that is gone is an error on that node.
- `editor.matgraph_function_users { path? }` lists the project's graphs that use the function file (default:
  the open function, with its unsaved signature): `{ path, signature, users: [{ path, function, calls,
  stale: [{ node, message }] }] }`. In the panel: a function tab's **Function users** section (press **List**;
  click a graph to open it in a tab of its own) shows the same list.

### Live function tabs (ADR-7730)

A graph that calls a function file open in a function tab compiles the tab's definition, saved or not: an edit in
the tab changes the calling graph's `editor.matgraph_state` (`key`, errors), `editor.matgraph_compile` code, preview
and node swatches at once, in every open graph that calls the file. The file itself changes only on save; closing
the tab without saving goes back to the file. A function tab is checked against its calling graph's open tab as it
is now (the `context` path's tab), else against the graph read when it opened.

A function tab has its own preview: `editor.matgraph_preview` (and the panel's preview, with the same shapes, orbit
and light) draws the calling graph with the tab's definition (its open tab, else its file). A function whose calling
graph was never saved previews its first output as an unlit color, with its inputs at zero.

```json
{ "method": "editor.matgraph_function_open", "params": { "path": "assets/materials/wear.matfunc.ron" } }
{ "method": "editor.matgraph_edit", "params": { "op": "set", "name": "k", "node": "Float(0.25)" } }
{ "method": "editor.matgraph_preview", "params": { "shape": "cube" } }
{ "method": "editor.matgraph_tabs", "params": { "select": 0 } }
{ "method": "editor.matgraph_compile", "params": { "include_wgsl": true } }
```

### Stale signatures

In a graph, a `Call` node's pins follow its function's signature (the file's, for a file function): one pin
per input in order, then any argument the function no longer has, marked stale. When the signature changed,
the node reads "stale signature" and `editor.matgraph_state` lists it in `stale: [{ node, function,
unknown_args, missing_args, bad_output, message }]` (also in `warnings`). The fix:

```json
{ "method": "editor.matgraph_function_fix", "params": {} }
```

answers `{ fixed: [{ node, rewired, dropped, unwired, output }], stale, revision, ok, errors }` in one undo
step. Arguments of inputs that still exist stay. When as many arguments lost their input as inputs lost their
argument, they pair up in order (old argument names sorted, new inputs in signature order), so a renamed
input keeps its wire; otherwise the stale arguments are dropped and the new inputs stay unwired. A call that
names an output the function no longer has gets its only output (or its first one). `nodes` limits the fix
to some calls. In the panel: **Fix signature** on the node.

### Collapse and expand

```json
{ "method": "editor.matgraph_function_collapse", "params": {
    "nodes": ["rgb", "tinted", "white", "base"], "name": "tint_blend" } }
{ "method": "editor.matgraph_function_collapse", "params": {
    "nodes": ["uv", "u"], "name": "coord", "file": "coord.matfunc.ron" } }
{ "method": "editor.matgraph_function_expand", "params": { "node": "base" } }
```

Collapse moves the selected nodes of the open graph into a new function. Its inputs are the nodes outside
the selection that it reads (each input, and the `Input` node that reads it, takes that node's name and its
inferred type); its outputs are the selected nodes something outside reads. Each output becomes a `Call`
node with that node's name and canvas position, so every reader keeps its wire. Without `file` the function
is inline; with `file` it is written to that project-relative `.matfunc.ron` (next to the graph) and the graph
references it. The answer is `{ function, inputs, outputs, calls, file, wgsl_identical, revision, ok,
errors }`: the compiled code is the code before, comments aside (`wgsl_identical: true`), with one output or
several: calls of one function with the same arguments share one expansion, so nodes the outputs share are
emitted once (ADR-7730). One undo step: undo also removes the file the collapse wrote.

Expand inserts the function nodes a `Call`'s output reads in its place (named as in the function when free,
else `<call>_<name>`), turns `Input` nodes into the call's arguments, and gives the output node the call's
name: `{ node, result, nodes, revision, ok, errors }`. The function stays declared. In the panel: Shift+click
node titles to pick nodes, then **Collapse into function…** in a node's menu; **Expand inline** on a `Call`
node; **Open function** opens a file function in a new tab.

## Instances

```json
{ "method": "editor.matgraph_instance_open", "params": { "path": "assets/materials/red_hull.matinst.ron" } }
{ "method": "editor.matgraph_instance_set", "params": { "param": "tint", "value": [0.9, 0.1, 0.1] } }
{ "method": "editor.matgraph_instance_set", "params": { "switch": "weathered", "value": false } }
{ "method": "editor.matgraph_instance_set", "params": { "texture": "albedo", "value": "textures/red.png" } }
{ "method": "editor.matgraph_instance_set", "params": { "param": "rough", "clear": true } }
{ "method": "editor.matgraph_save", "params": {} }
```

An instance opens in its own tab on its parent's graph (the `parent` path, next to the instance first),
read-only: an edit of the graph is `matgraph_read_only`. Overridden parameters are highlighted on their
`Param` nodes and edited there or in the side panel's **Instance overrides**; `editor.matgraph_state` marks
them on the node (`override`, `default`). `editor.matgraph_instance_open { path, create?, parent? }` answers
`{ path, tab, parent, overrides, ok, revision }`; `create: true` with `parent` starts an instance with no
overrides. `editor.matgraph_instance_set` takes exactly one of `param`, `texture` or `switch` with a `value`
(a parameter value is `{ "Float": 0.5 }`, `{ "Color": [r, g, b] }`, RON text, or a number or array of the
parameter's width) or `clear: true`, and answers `{ revision, ok, errors, overrides }`. Overrides are checked
against the parent (declared, same type, finite). `editor.matgraph_save` writes the `.matinst.ron` only, in
the form the instance API writes (`MaterialInstance::to_ron`); the parent graph is never written. Compile and
preview show the parent with the overrides.

### Errors of these methods

| Code | When |
|---|---|
| `matgraph_collapse_invalid` | An empty selection, a missing node, a bad or taken function name, nothing outside reads the selection, inputs that do not compile, a function file path outside the project or one that exists |
| `matgraph_expand_invalid` | The node is not a `Call`, its function cannot be read, or it names an output the function does not have |
| `matgraph_read_only` | An edit of an instance's graph (only its overrides change) |
| `matgraph_instance_invalid` | An override the parent does not declare or whose type differs, a missing parent, or an instance method on another kind of document |

## Stage 2: shapes, typed rows, search, clipboard, comments, swatches, Apply (ADR-9700)

**Preview shapes.** `editor.matgraph_preview` takes `shape` (`sphere`, `cube`, `plane`, `mesh`), `mesh` (a project
`.glb` / `.gltf`: its first part, centered and scaled to the frame, at most 250 000 vertices), and `yaw`, `pitch`,
`light_yaw` in radians (the panel's orbit drag and light slider). A mesh that cannot load is
`matgraph_preview_mesh_failed` with the reason; the panel keeps its last image and shows the reason.
`editor.material_preview { path, shape?, mesh?, yaw?, pitch?, light_yaw?, width?, height? }` renders a
`.material.ron` the way the Material panel does and answers `{ path, shape, width, height, png, mean_rgb,
center_rgb, coverage }`. The Material panel itself renders its preview only when the material, the shape, the
mesh, the orbit, the light turn or the image size changed, not on every repaint (ADR-9970).

```json
{ "method": "editor.matgraph_preview", "params": { "shape": "mesh", "mesh": "assets/props/crate.glb", "yaw": 0.6 } }
```

**Typed rows.** `editor.matgraph_node_fields { name }` lists the inspector's rows of a node: `{ name, kind, value,
options? }` with kinds `float`, `vec2`..`vec4`, `int`, `param`, `texture`, `switch`, `input`, `function`,
`call_output`, `channel`, `normal_blend`, `channels`; pickers list their `options`, and a `Param` node also answers
its declared `default`. `editor.matgraph_node_set { name, field, value }` sets one row (one undo step; repeated sets
of a row coalesce like a drag): a number, an array of numbers, a whole number, a name, or `null` for a `Call`'s
output. `field: "default"` on a `Param` sets the declared default. `Noise` octaves clamp to 1..8; a bad enum name or
swizzle is refused. The typed rows and RON (`editor.matgraph_edit set`) give the same node.

**Search.** `editor.matgraph_search { query, from? | to + slot? | output?, add?, pos? }` answers the kinds that
match, best first (fuzzy: `lrp` finds `Lerp`), filtered to those that can take the wire: from a node's output a kind
needs an input; into an input or a surface output any kind works. With `add` it adds that kind at `pos`, wired, and
answers `added` (the panel's Space / Tab search and the menu of a wire dropped on empty canvas do the same).

The read-only twin is `editor.matgraph_find { query, from? | to + slot? | output? }` (rev-claude-47-k finding 3):
the same listing — identical `kinds` for the same parameters — with no `add` and no `pos`, registered
non-mutating (`Permission::Read`), so an agent whose profile withholds `edit_scene` can still list search results.
Passing `add` to it is refused with `F0002` and a message pointing at `editor.matgraph_search`; the mutating method
itself is refused wholesale for such a profile, `add` or not.

**Clipboard.** `editor.matgraph_clipboard { op: "copy" | "cut", nodes }` answers the clipboard `text` (RON of the
nodes and their positions relative to the selection's corner); `{ op: "paste", pos?, text? }` pastes it (or the
editor's clipboard, shared by every tab) and answers `renamed` (copied name → pasted name). A name the document
already has becomes `name_2`, … and the copies' references follow; a reference to a node outside the copy is left
unwired. Cut and paste are one undo step each.

**Arrange.** `editor.matgraph_arrange { op, nodes }`: `align_left`, `align_right`, `align_top`, `align_bottom`,
`align_center_x`, `align_center_y` (two or more nodes), `distribute_x`, `distribute_y` (three or more).

**Comments and reroutes.** `editor.matgraph_annotate { op }`: `comment_add { text, min, max }` (answers `index`),
`comment_set { index, text?, min?, max? }`, `comment_remove { index }`, `reroute_set { to, slot, points }` (an empty
list clears). They are saved in the layout sidecar only (`.matgraph.layout.ron` v2; v1 files load with none) and
each change is one undo step.

`comment_set` also takes `rect: [[x, y], [x, y]]` instead of `min` and `max`, and `move_nodes: true` to move the
nodes inside the old frame with it (a node is inside when its canvas position, its top-left corner, lies in the
frame): it answers `moved`, the names moved, and the frame and its nodes are one undo step. This is the agent twin of
dragging a frame's header on the canvas (ADR-9970).

**On the canvas (ADR-9970).** A rerouted wire is drawn as one curve from its output pin through its points, in
order, to its input pin (a Catmull-Rom curve through every point); the canvas's node widget has no wire of its own for
that pin pair. Drag a point to move it, double-click it to add a point after it, right-click it to remove it; a drag
is one undo step (agent twin: `reroute_set`). Click a wire to add a reroute point where you clicked (on a rerouted
wire, on the span you clicked); right-click a rerouted wire to disconnect its input (a plain wire disconnects on a
right-click too). One undo step each; agent twins `reroute_set` and `editor.matgraph_edit { op: "disconnect" }`
(ADR-7730). A comment frame's header strip moves the frame and the nodes inside it;
its edges and corners resize it; double-click the header to edit its text (Enter keeps it, Escape drops it). One undo
step per drag. The Comments and reroutes list in the side column stays.

**Node swatches.** `editor.matgraph_node_preview { node, size? }` renders one node's value: the node routed to an
unlit base color in a derived graph (a float as gray, a `Vec2` as red/green, a `Vec4`'s RGB) on the preview sphere.
It answers `{ node, size, png, mean_rgb, gray_levels, key }`; `gray_levels` counts distinct levels in the center (a
`Noise` node has many, a constant a few). The panel's Node previews toggle shows these on the canvas, rendered
only after an edit, a few per frame, cached by pipeline key; the cache keeps at most 256 swatches and drops the
least recently drawn first (ADR-7451), but never one of a node on screen: with more than 256 nodes visible it holds
every visible swatch and drops only off-screen ones (ADR-7730). Live debugging of values in a running game is not
offered.

**WGSL view.** The side column's **WGSL** section shows the compiled code, read-only, with a **Copy** button: the
`wgsl` text `editor.matgraph_compile { include_wgsl: true }` answers (default switches; a function in its
harness), then the vertex code when the graph offsets vertices (`vertex_wgsl`).

**Apply to selection.** `editor.matgraph_apply_to_selection {}` sets the selected meshes' `MeshSource.material` to
the open graph or instance's saved file (project-relative), as one undo step, and answers `{ path, owners }`. It is all or nothing: when one owner cannot
take the material, no owner changes and no undo step is made (ADR-7451). Select
meshes first (`editor.select`). New graphs, functions and instances: `editor.create_asset { kind: "material_graph" |
"material_function", path }` and `{ kind: "material_instance", path, parent }`, the Content Browser's New menu
(book `agents/content-browser.md`).

### Errors of the Stage 2 methods

| Code | When |
|---|---|
| `matgraph_preview_mesh_failed` | The preview mesh is not `.glb` / `.gltf`, is outside the project, does not load, has no triangles or more than 250 000 vertices |
| `matgraph_node_missing` | `editor.matgraph_node_preview` names a node the document does not have |
| `matgraph_preview_failed` | A node's swatch cannot compile or render, or the node is in a function tab |
| `matgraph_apply_refused` | No graph or instance open, a function tab, a document never saved or outside the project, nothing selected, or no mesh in the selection |
| `material_preview_failed` | `editor.material_preview`'s render failed |
| `matgraph_edit_refused` | A typed row the node does not have, a value the row refuses, an empty clipboard, too few nodes to arrange, a comment index or reroute slot that does not exist |

## Limits

Collapse works in a graph, not inside a function (a function cannot declare functions). A function tab draws no
node swatches (`editor.matgraph_node_preview` there is `matgraph_preview_failed`). An error inside an expansion two
identical calls share is shown on the first call. The preview draws its shape at rest (no world position offset
motion).

## Preview scene (ADR-7771)

The Material editor's preview, the mesh viewer and the texture viewer share one preview scene
(`regia_editor::preview_scene`): a **lighting preset**, an **environment intensity**, a **floor with a
grid** and an **auto-rotate** turntable. The controls sit under the shape buttons (Material editor) and
over the render (viewers); each is a focusable widget (Tab, then Space, Enter or the arrows), named for a
screen reader, sized by `appearance.ui_scale` and colored from the theme.

| Preset | What it is |
|---|---|
| `outdoor` | The editor's own sun over the procedural sky. The default, and what the previews always drew. |
| `studio` | Three-point light: a key, a weaker fill from the other side and a rim from behind, on a dark backdrop. |
| `neutral` | A flat gray environment with a weak key light: shows color and form, no mood. |
| `level` | The open level's sun (its directional light: the one the renderer takes as the sun) over the sky. With no directional light in the level, `outdoor` is drawn (`effective_preset` says so). |

`intensity` (0 to 4, default 1) scales the preset's ambient light. `floor` draws a gray floor with a grid
at the subject's lowest point. `rotate` turns the subject about the vertical axis, one turn in 12 seconds,
while lights and floor stay put. With the defaults the picture is the one from before the scene existed.

`editor.preview_scene { editor, preset?, intensity?, floor?, rotate? }` reads (only `editor`) or changes the
scene of `editor` (`material`, `mesh`, `texture` or `matgraph`, the material graph editor: w13) and answers `{ editor, preset, intensity, floor, rotate,
effective_preset, level_lighting }`; the panels take a change on their next frame. A refused call changes
nothing. Verify with `editor.screenshot` of the editor window, or `editor.material_preview`
(which draws the default scene).

| Code | When |
|---|---|
| `invalid_params` | `editor` missing or not `material`, `mesh`, `texture`; `preset` not `outdoor`, `studio`, `neutral`, `level`; `intensity` outside 0 to 4 or not a number; `floor` or `rotate` not a boolean. |

Hooking the material graph editor in: `crate::preview_scene::preview_scene_controls(ui, &mut ctx.settings)`
beside its shape controls, `ctx.tick(ui.ctx())` once per frame, and
`crate::material_editor::render_preview_scene(device, &lab, &geometry, &view, size, key, &ctx)` where it calls
`render_preview` (add `ctx.key()` to its render key so it redraws on change).

Tests: `cargo test -p regia_editor --lib preview_scene material_editor` (the rigs, the floor, the turntable and
the agent hand-off need no GPU; `the_preview_scene_controls_change_the_picture` needs lavapipe or a GPU).


## Parameters, reparenting, stats, tiers and usages (w12, ADR-7851)

**Parameters panel.** A graph tab lists its parameters, textures and static switches in slot order, each with its
type, a typed default (drag values, a color button, an image path, a checkbox) and the nodes that read it. Drag a
row's name onto another row of the same list to move it there, or use its **Up** and **Down** buttons (the keyboard
route); **Remove** drops a declaration (the nodes that read it stay, and the compiler names them); the Add row takes
a name and a type. The slot order is the uniform layout, so moving a parameter changes the generated code; instances
follow by name and list the parent's parameters in this order. The shading block keeps its RON editor.

```json
{ "method": "editor.matgraph_params", "params": {} }
{ "method": "editor.matgraph_params", "params": { "op": "set", "name": "rough", "value": 0.25 } }
{ "method": "editor.matgraph_params", "params": { "op": "set", "kind": "switch", "name": "wet", "value": true } }
{ "method": "editor.matgraph_params", "params": { "op": "reorder", "kind": "param", "name": "rough", "to": 0 } }
{ "method": "editor.matgraph_params", "params": { "op": "remove", "kind": "texture", "name": "detail" } }
```

`list` answers `rows: [{ kind, index, name, type, default, used_by, category, tooltip, range }]` and `groups`;
`set` adds `added` (a new declaration goes at the end of its list), `remove` adds `readers`. Each change is one undo
step. Categories, ranges and tooltips (the v5 fold, w13) are described below, under "Parameter categories, ranges
and tooltips".

**Reparent.** An instance tab has a **New parent** picker of the project's graphs and a **Reparent** button. The
instance takes the new parent's graph and layout; overrides the new parent declares with the same type are kept, the
others are dropped and named in the status line. One undo step; save writes only the `.matinst.ron`. An instance's
parent is a graph (the `.matinst.ron` format has no instance-of-instance chain).

```json
{ "method": "editor.matgraph_instance_reparent", "params": { "parent": "assets/floor.matgraph.ron" } }
```

answers `{ parent, kept, dropped, revision, ok, errors }`.

**Hierarchy.** The side column's Hierarchy section lists every graph of the project with the instances whose parent
resolves to it, and the instances whose parent is missing. A click opens a file (a graph in a tab of its own);
dragging an instance onto a graph opens the instance and reparents it (save to keep it).

```json
{ "method": "editor.matgraph_hierarchy", "params": {} }
```

answers `{ graphs: [{ path, name, instances: [{ path, parent }] }], orphans }`.

**Stats per profile.** Under the preview, a Stats strip counts the colour pass of the preview's tier (Desktop High
for the editor's own tier): IR instructions (expressions and statements of the fragment entry and every function it
calls), how many of them the graph's own code has, texture samples, bindings and samplers. The numbers come from the
composed naga module, so they are the same on every machine; they change when the code changes, not when a
parameter value does. The Permutation policy section's **Count** lists every profile. The agent twin lists every
profile the permutation policy enumerates, or one:

```json
{ "method": "editor.matgraph_cost", "params": {} }
{ "method": "editor.matgraph_cost", "params": { "profile": "MobileLow" } }
```

Each row: `{ profile, instanced, passes, instructions, graph_instructions, texture_samples, graph_texture_samples,
bindings, samplers, error }`.

**Tier preview.** The preview's **Tier** picker draws the material with a render profile's frame quality (the
profiles the policy enumerates, or Editor); the badge beside it names the tier the image shows.
`editor.matgraph_preview` takes `profile` (and answers it), and `node` to draw one node's value instead of the
material.

**Node menu.** A node's context menu adds **Find usages** (parameter, texture, switch, function-call and constant
nodes), **Preview this node** (the node's value pinned in the preview until **Unpin**) and **Disconnect all**
(its inputs, the inputs that read it and the outputs it drives, one undo step). The same three are buttons under the
selected node in the side column (the keyboard route). Find usages lists the project's graphs and functions that
declare, read or call the same item and the instances that override it or inherit it from such a graph; a click
opens the file.

```json
{ "method": "editor.matgraph_find_usages", "params": { "kind": "param", "name": "tint" } }
{ "method": "editor.matgraph_find_usages", "params": { "node": "wobble_call" } }
{ "method": "editor.matgraph_edit", "params": { "op": "disconnect_all", "name": "mix" } }
```

`find_usages` answers `{ kind, name, usages: [{ path, doc, nodes, how }] }` (`doc`: graph, function or instance;
`how`: declares, reads, calls, overrides, inherits). A function call matches the same `.matfunc.ron` under any name.

### Errors of the w12 methods

| Code | When |
|---|---|
| `matgraph_params_invalid` | Unknown op or kind, a missing `name`, `to` or `value`, a bad name or default, a declaration that does not exist |
| `matgraph_read_only` | `editor.matgraph_params` changes something in an instance or a function tab |
| `matgraph_reparent_invalid` | No instance open, a parent that does not exist or is outside the project |
| `matgraph_profile_unknown` | `profile` is not one of the permutation policy's profiles |
| `matgraph_cost_failed` | The document does not compile |
| `matgraph_usage_invalid` | No `kind` and `name` or `node`, an unknown kind, or a node that names nothing searchable |

## Code node: WGSL in a node (ADR-7852)

A **Code** node (`.matgraph.ron` v5) holds the body of one WGSL function. Its parameters are its typed inputs in
order, then each texture input as a `texture_2d<f32>` of its name and the graph's sampler as `material_sampler`; it
returns its declared output type (`Float`, `Vec2`, `Vec3` or `Vec4`). Add one with `editor.matgraph_edit { op:
"add", kind: "Code", name }` (it starts with one `Float` input `a` and `return a;`), wire its inputs as any other
node's (the slot names are the input names) and set its text:

```json
{ "method": "editor.matgraph_code_set", "params": {
    "node": "mad",
    "inputs": [{ "name": "a", "ty": "Float" }, { "name": "b", "ty": "Float" }, { "name": "c", "ty": "Float" }],
    "output": "Float",
    "code": "return a * b + c;" } }
```

`code`, `inputs`, `textures` and `output` are each optional; an input keeps its wire by name (or, after a rename, by
position). Consecutive sets of the same node merge into one undo step, as the panel's applies do. The answer is `{ node, code_ok, code_errors, revision, ok, errors }`; each code error is `{ code, line,
column, include, message }`, with `line` and `column` 1-based in the node's own text (line 0 is the declaration).
`editor.matgraph_code_errors { node? }` lists the same for one Code node or every one:

```json
{ "method": "editor.matgraph_code_errors", "params": { "node": "mad" } }
→ { "nodes": [{ "node": "mad", "ok": false, "errors": [
      { "code": "code_syntax", "line": 2, "column": 12, "include": null, "message": "expected expression, found \";\"" }] }] }
```

Rules the check enforces before any pipeline is built (naga parses and validates the node as its own module):
no entry points, no module-scope variables (so no bindings and no textures except the texture inputs), no `override`
constants, braces in the body balance, input names are identifiers that do not start with `mg_` or `__`. At most
16 KiB of code, 16 inputs and 8 includes. A Code node with texture inputs cannot feed `world_position_offset`.

`#include "name.wgsl"` on its own line reads `shaders/include/name.wgsl` from the project (a plain file name, no
path). The functions it declares can be called from the body. The include's text is only read into the copy that
compiles: the document keeps the line, and editing the include file recompiles the graph (the cook hashes it too).

While a Code node fails, the preview shades it magenta (a constant of its output type) and the graph's errors list
it; `editor.matgraph_compile` and the cook refuse the graph with the same message (`code line L, column C: …`).

The panel: select a Code node to get its declaration rows (input name and type, **Remove**, **Add input**,
**Returns**, texture inputs), then the code view: line numbers (a `!` marks a line with an error), keyword and type
colors, the bracket at the cursor and its match highlighted, **Find** (Ctrl+F; Enter goes to the next match, Esc
closes it), the errors as a list (click or press Enter on one to move the cursor to its line and column), and
**Apply** (Ctrl+Enter). The text also applies itself after a short pause (30 frames without a keystroke); each apply
is one undo step, and applies to the same node in a row merge. Under the text, the identifiers that complete the
word before the cursor (the node's inputs, the include files' functions, the common WGSL built-ins): click one, or
press Ctrl+Space for the first.

### Errors of the Code node methods

| Code | When |
|---|---|
| `matgraph_node_missing` | `node` is not a node of the open document |
| `matgraph_not_code_node` | `node` is a node of another kind |
| `matgraph_code_bad_type` | An input `ty` or the `output` is not `Float`, `Vec2`, `Vec3` or `Vec4` |
| `matgraph_edit_refused` | The document refuses the edit (an instance tab is read-only) |

Code diagnostics (`code_errors[].code`): `code_syntax` (naga could not parse it), `code_invalid` (naga's validation:
types, unknown names, a missing return), `code_restricted` (an entry point, a binding or module-scope variable, an
`override`, unbalanced braces), `code_include` (a bad or missing include), `code_signature` (a bad or repeated input
name, an undeclared texture input, more than 16 inputs), `code_too_large` (more than 16 KiB of code or 64 KiB in one
include).

## Parameter categories, ranges and tooltips (w13, ADR-8105 (b))

`.matgraph.ron` v5 gives each parameter three optional fields (all editor-only: compile ignores them, so the
generated WGSL and the cooked material are the same with or without them):

| Field | Default | Meaning |
|---|---|---|
| `category` | `""` | The group the Parameters panel lists it under. Ungrouped parameters come first, then the categories in order of first appearance; inside a group the slot order holds. |
| `tooltip` | `""` | Shown under the row and on hover, in the Parameters panel and in an instance's overrides. |
| `range` | none | `(min, max, step)`, `step` 0 = continuous. Edits of the default and instance overrides are clamped to it and snapped to the step (every component of a vector or color). The runtime material API does not clamp. |

The loader refuses a non-finite bound, `min >= max`, a step outside `0..=max - min`, or a default outside the range,
naming the parameter. A v4 file reads with no metadata; a parameter without metadata writes as before.

**Panel.** A ranged `Float` is a slider (the arrow keys step it), other ranged types are value boxes bounded by the
range. **Up** and **Down** move a parameter past its group's neighbour. Each parameter's **`<name>` details**
section edits its Category and Tooltip (committed when the field loses focus) and its **Range** (checkbox, then Min,
Max and Step); a new range clamps the default. An instance tab lists its overrides under the parent's categories,
bounded by the parent's ranges, with the parent's tooltips.

```json
{ "method": "editor.matgraph_params", "params": { "op": "meta", "name": "rough", "category": "Surface", "tooltip": "How rough", "range": { "min": 0, "max": 1, "step": 0.05 } } }
{ "method": "editor.matgraph_params", "params": { "op": "meta", "name": "rough", "range": null } }
```

`meta` changes the parts it names (at least one of `category`, `tooltip`, `range`) in one undo step and answers the
rows, the `groups` (`[{ category, params }]`, panel order) and `value` (the default after the change). Errors:
`matgraph_params_invalid` (no part named, a bad range, a category over 64 or a tooltip over 512 characters, a
control character, an unknown parameter).

## Preview scene and Problems (w13)

The editor's preview has the shared preview scene's controls (lighting preset, environment intensity, Floor,
Rotate) under the shape buttons; they show without a GPU too, and the scene is shared by every tab. Drive it with
`editor.preview_scene { editor: "matgraph", … }`; `editor.matgraph_preview` draws the editor's scene with the
turntable at rest (and `editor.material_preview` draws the Material editor's scene the same way). With the default
scene the picture is the one from before.

Compile errors of every open document (the active tab live, parked tabs as they were parked) are listed in the
Problems panel under **Material graphs**, each with an **Open** button (named "Open `<target>` in `<tab>`") that
activates the tab, selects the node and brings the Material Graph panel forward. The agent twin is
`editor.problems_panel`: its answer has `graph_errors: [{ index, tab, label, path, kind, target, node, message }]`,
and `open_graph_error: <index>` follows a link (answer `opened_graph_error`; error `graph_error_missing`, see
[Project health](project-health.md)).

Tests: `cargo test -p regia_matgraph --lib a_v4_graph_upgrades_with_default_param_metadata`;
`cargo test -p regia_editor --lib matgraph_editor::w13_tests` (the preview half needs lavapipe or a GPU).
