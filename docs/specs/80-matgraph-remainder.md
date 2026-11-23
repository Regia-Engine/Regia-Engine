# 80 — Material graph editor (T2.4 remainder)

Status: proposed (ADR-2318). Version line: v0.4.0 "content creation" (`docs/versions.md`), master plan row
"Material graph + editor" (`docs/master-plan.md`). Parent spec: `specs/22-tier2.md` T2.4; contract: `specs/13` § 7.

## 1. Purpose

`regia_matgraph` already has everything T2.4 asks for on the engine side: the `.matgraph.ron` format (v2: math,
texture sampling, UV ops, time, world/view vectors, Fresnel, parameters, functions, static switches, world position
offset), compile errors that name their node, instances, the permutation cache, the cooked form, the runtime and
the `material.graph_*` agent methods. What T2.4 still lacks is the **editor**: "a node graph producing WGSL ...
live preview + compile errors mapped back to nodes". This spec adds that editor and nothing in the runtime.

## 2. What is built

A desktop-only editor module `regia_editor::matgraph_editor` (the editor is desktop-only, AGENTS.md § 5):

1. **Document model** (`model.rs`, pure, no egui): the open `MaterialGraph` plus canvas positions, with typed
   edit operations — add, replace, remove, rename (references follow), connect, disconnect, set or clear a surface
   output, move, replace the params / textures / switches / shading blocks. Operations refuse bad names; a graph
   that does not compile is still a valid document (the errors are shown, not enforced), as with
   `material.graph_set`.
2. **Pin model** (in `regia_matgraph::pins`): `Node::input_slots()` names every input slot of a node kind in
   order (`["a", "b", "t"]` for `Lerp`, the optional `lod` of `Sample`, a `Call`'s argument names), and
   `Node::set_input(slot, value)` rewires one. `inputs()` / `map_inputs()` are unchanged. A disconnected required
   input holds the empty name `""`, which the compiler already reports as an error on that node.
3. **Diagnostics**: every edit compiles the graph (`compile_unvalidated_with`, the fast path, no shader
   composition) and maps each `CompileError` to a canvas target: a node (`"rgb"`), the node a function-expansion
   error came from (`"worn/out"` → `"worn"`), a declaration (`"param:tint"`), an output slot (`"output:normal"`),
   or the whole graph. The canvas paints a node with errors red and lists its messages under it.
4. **Undo**: each edit is a snapshot command (`MatGraphEdit`) on the editor's shared `UndoStack`, so `editor.undo`
   and Ctrl+Z revert graph edits in order with level edits. Node drags coalesce into one entry per node. An entry
   belongs to the document it was made in: after another graph is opened it does nothing, so an undo cannot write
   one graph's snapshot over another. Opening over unsaved work: the panel warns on the first press of New or Open,
   and `editor.matgraph_open` answers `discarded_unsaved: true`.
5. **Canvas** (`panel.rs`): an egui-snarl canvas (`egui-snarl` is already a workspace dependency, used by the
   anim graph editor). One output pin per node; one input pin per slot; a **Surface** sink node with the eight
   surface outputs. Right-click the canvas to add a node by kind; right-click a node to delete, duplicate or wire
   it to an output. The side panel edits the selected node as RON (any field of any kind), the declaration
   blocks as RON, and shows the live preview.
6. **Live preview**: the graph's material on the built-in sphere through the editor's frame renderer (the same
   renderer the Material panel and thumbnails use; inline pipeline build, so the first preview frame draws the
   graph). The preview compiles with full validation (`compile_with`); textures load from the project. A preview
   that cannot compile keeps the last good image and says why.
7. **Noise node (format v4)**: T2.4 lists noise, which no node provided. `Noise { input, octaves }` is
   fractal gradient noise (`Float`, 0..1) at a `Float`, `Vec2` or `Vec3` position, 1–8 octaves. The gradient
   lattice uses an integer hash of the cell (no permutation table, no texture), so every device and both
   stages pick the same lattice gradients (the blend is float math); it may feed `world_position_offset`.
   `Wind` is the v3 node. `Noise` is the v4 node: `.matgraph.ron` is v4 and
   `.matfunc.ron` (which holds nodes too) is v3; the `3 → 4` and matfunc `2 → 3` steps only move the version (migrators registered, rows in
   `regia upgrade-project`). Graphs without noise generate the same code and pipeline keys as before.
8. **Layout sidecar** `<graph>.matgraph.layout.ron`: `(format_version: 2, positions: { "node": (x, y) }, comments:
   [...], reroutes: [...])`. Editor-only, never cooked, safe to delete (nodes without a position are laid out by
   depth). It follows the `.animgraph.layout.ron` precedent; it is not a frozen content format (`specs/12` § 4a
   lists content the runtime reads). v2 (ADR-9700) added comment frames and edge reroute points with serde
   defaults, so v1 files load with neither; a reader seeing a newer version ignores the file and lays out by depth.

## 3. API shape

```rust
// regia_matgraph::pins (new, additive)
impl Node {
    pub fn input_slots(&self) -> Vec<(String, Option<&str>)>;     // (slot name, wired node or None)
    pub fn set_input(&mut self, slot: &str, node: Option<String>) -> Result<(), String>;
    pub fn default_of(kind: &str) -> Option<Node>;                  // a new node of a kind, inputs unwired
    pub const KINDS: &[&str];                                       // every kind `default_of` accepts
}
impl SurfaceOutputs {
    pub const SLOTS: [&str; 8];
    pub fn get(&self, slot: &str) -> Option<&str>;
    pub fn set(&mut self, slot: &str, node: Option<String>) -> Result<(), String>;
    pub fn rename(&mut self, from: &str, to: Option<&str>) -> usize;   // rewire or clear outputs naming `from`
}

// regia_editor::matgraph_editor
pub struct MatGraphDoc { pub graph: MaterialGraph, pub layout: BTreeMap<String, [f32; 2]> }
pub struct MatGraphEditor { pub path, pub doc, pub revision, pub diagnostics, pub toast, .. } // world resource
pub struct Diagnostics { pub ok: bool, pub errors: Vec<Diagnostic>, pub key: Option<u64>, pub graph_hash: Option<u64>,
                         pub warnings: Vec<String>, pub signatures, pub stale }   // for_target(&Target) -> messages
pub struct Diagnostic { pub target: Target, pub source: Option<String>, pub message: String }
pub enum Target { Graph, Node(String), Param(String), Texture(String), Switch(String), Function(String), Output(String) }
pub fn edit(world: &mut World, label: &str, merge_key: Option<String>, f: impl FnOnce(&mut MatGraphDoc) -> Result<R, String>) -> Result<R, String>;
pub fn layout_path(graph: &Path) -> PathBuf;
pub fn matgraph_editor_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>);
```

Refined by the code (ADR-5814): `Diagnostics` keeps the errors in compiler order, each with its canvas target and the
compiler's own target string (`"worn/out"`), and answers per target through `for_target`; it also carries the
graph hash, the warnings (switch count, stale calls) and the § 11 signatures and stale calls. `Target` also names a
static switch declaration (`"switch:x"`) and a function definition (`"function:x"`).

## 4. Agent-protocol surface

Editor-side methods (`regia_editor::matgraph_editor::agent`, page `book/agents/material-graph-editor.md`):

| Method | Does |
|---|---|
| `editor.matgraph_open { path?, create? }` | Open a `.matgraph.ron` (and its layout) or start a new graph |
| `editor.matgraph_state { ron? }` | Nodes with kind, position, input slots and their errors; outputs; declarations; `ok`, `errors`, `dirty`, `revision` |
| `editor.matgraph_edit { op, ... }` | One edit: `add`, `set`, `remove`, `rename`, `connect`, `disconnect`, `output`, `move`, `params`, `textures`, `switches`, `shading` |
| `editor.matgraph_compile { include_wgsl?, switches? }` | Full compile with validation: `key`, `graph_hash`, param and texture slots, errors by node, permutation count, the shipped permutation policy per profile (`policy`, § 6) |
| `editor.matgraph_preview { width?, height? }` | Render the preview sphere: PNG (base64), mean and center color |
| `editor.matgraph_save { path? }` | Save graph and layout atomically |

Mutating methods use `Permission::EditScene` and land on the undo stack. Errors use the shared codes `F0002` (bad
parameters, unknown op), `F0003` (no graph open) and `F0011` (no GPU for the preview), plus the method codes
`matgraph_edit_refused` (an edit the model refuses), `matgraph_preview_failed` and `matgraph_save_failed`.

## 5. Determinism

Edits are pure functions of the document; the compile is deterministic (`graph_hash` and `key` are structural).
Layout positions for nodes without one are a function of the node's depth from the outputs and its name order,
not of map iteration order or time. The preview uses the editor frame quality (no jitter) and time 0.

## 6. Shader-permutation policy (`master-plan` § 5.5a)

The editor adds no shader axis: a graph is one `material_surface` splice per permutation, and permutations come
only from static switches. `editor.matgraph_compile` reports `permutations` (2^switches, the bound the cook's
enumeration will list, saturating) and refuses no graph; the state and the canvas warn when a graph declares more
than `PERMUTATION_WARN_SWITCHES` = 4 switches (16 permutations), the point where one graph alone takes a large share
of a profile's pipeline budget.

The policy itself shipped in v0.2.8 (ADR-2026, `regia_render::permutation_policy`): the cook lists every graph
pipeline set per render profile, `ship_check` holds each profile to its budget, and a shipping build draws an
unlisted graph with the standard surface. The editor keeps that policy in force and does not add to it:
`editor.matgraph_compile` answers `policy`, the shipped `enumerate_profile` run over the open graph's own
permutations (every switch combination, at most `POLICY_MAX_PERMUTATIONS` = 64) for each real-time preset of a
desktop or Android package (`platform_presets`), with that profile's default `PermutationBudget` and
`CompileCostModel`. It reports per profile the sets, pipelines, modules, estimated cold compile time and whether
this graph alone would exceed the budget (`model::policy_cost`). Recut on v034 (2026-11-02): this paragraph replaces
the earlier "out of scope" note, which predates the shipped policy.

## 7. Tests and acceptance

- `regia_matgraph`: `Noise` in both stages passes naga and `spirv-val` (`all_nodes`), octave and input-type
  errors name the node; a v2 graph and a v1 function keep their nodes and load at the current versions
  (matgraph v4, matfunc v3).
- `regia_editor` preview: a noise base color gives many gray levels on the sphere, a constant few (GPU, skips
  without one).
- `regia_matgraph` unit tests: `input_slots` and `set_input` cover every kind; `set_input` then `inputs()` agree;
  `default_of` for every name in `KINDS` round-trips through RON; `SurfaceOutputs::get/set` for every slot.
- `regia_editor` unit tests (model): add/connect/disconnect/remove/rename keep references consistent; rename
  rewrites outputs and inputs; a removed node's dependants carry an error naming them; diagnostics map
  a `worn/out` error to the call node `worn` (the prefix is the call node's name); layout sidecar round-trips; open → edit → save → reopen is identical.
- Agent tests: open a fixture, wire a graph through `editor.matgraph_edit`, `editor.matgraph_state` reports
  `ok: true` and the edges; `editor.undo` reverts the last edit; a broken edge reports the node; `save` writes both
  files; `editor.matgraph_preview` renders a lit sphere on lavapipe (skips with no adapter).
- No golden images: the preview test checks coverage and that a red base color reads red.
- Recut on v034 (2026-11-02): `regia_matgraph` `default_graphs_unchanged` pins the WGSL digests and pipeline keys
  of the shipped graphs (the three asset graphs, the format example, an inline-function graph in both switch
  states) to their v034 values, so the v3 bump is proven not to move a default material; `policy_cost` runs the
  shipped enumeration per profile; a headless panel test drives the agent (open, add, connect, compile, save,
  reopen) and draws the panel over the same document; agent paths outside the project and graph texture or
  function paths with `..` or an absolute path are refused.

## 8. Platforms

Linux, Windows: the editor builds as today. Android aarch64: the editor is not built; `regia_matgraph`'s new
`Node` helpers are plain Rust and build everywhere (the Android check `cargo ndk -t arm64-v8a check -p regia`
covers `regia` and does not include the editor). macOS: testing only, no change.

## 9. Performance budget

The fast compile on each edit is under 1 ms for a 50-node graph (the `graph_build_speed` test bounds the compile
path already); it runs only when the revision changes, never per frame. The preview renders only when the
compiled key, parameters or panel size change (≤ 1 render per edit, editor quality, 256² default). The panel
draw has a `tracing` span.

## 10. Out of scope

Further node kinds (Voronoi / cellular noise, gradient-of-noise normals), the cook-time pipeline enumeration
and budget (§ 6), and live debugging of graph values in a running game (PIE). Per-node previews (§ 12) are the
editor's view of intermediate values (ADR-9700). (The function canvas and instance editing in the canvas, listed
here before, are § 11; the preview on custom meshes is § 12.)

## 11. Function canvas, instances in the canvas, collapse and expand (ADR-2490)

Added on v036 (2026-11-04), editor only (no format change):

1. **Tabs.** The editor holds several documents (graph, function, instance), one tab each; undo follows the
   tab its step was made in. `editor.matgraph_open` replaces the active tab as before.
2. **Function canvas.** A `.matfunc.ron` opens with its `Input` nodes and an **Outputs** sink; the node tools
   are the graph's. It compiles in a generated harness that borrows a calling graph's declarations (`context`,
   default the graph tab it was opened from); errors map to its nodes and outputs. The signature (inputs,
   output names) is one undoable edit; save writes the function and its layout sidecar.
3. **Signatures in callers.** A `Call` node's pins follow its function's signature (file functions too). A call
   that no longer fits is stale (a warning on the node and in `stale`); **Fix signature** rewires it (one
   undo step). `editor.matgraph_function_users` lists the project's graphs that use a function file.
4. **Collapse / expand.** A selection becomes a function (inline or a new `.matfunc.ron`), replaced by one
   `Call` per output that keeps the output node's name; the compiled WGSL of a single-output collapse equals
   the code before (comments aside). Expand is the inverse. Each is one undo step (a written function file
   included).
5. **Instances.** A `.matinst.ron` opens on its parent's graph read-only; overridden parameters are
   highlighted and edited on their `Param` nodes and in the side panel; save writes the instance file only,
   exactly as `MaterialInstance::to_ron` writes it. (Built since, w8-editor-smalls-z: the Content
   Browser's New ▸ **Material instance** picks the parent graph and creates the file with exactly those
   bytes — `editor.create_asset { kind: "material_instance", path, parent }`; § 11.5 of the book page
   `book/agents/content-browser.md`.)
6. **Agent.** `editor.matgraph_function_open/_signature/_collapse/_expand/_fix/_users`,
   `editor.matgraph_instance_open/_set`, `editor.matgraph_tabs`; codes `matgraph_collapse_invalid`,
   `matgraph_expand_invalid`, `matgraph_read_only`, `matgraph_instance_invalid`. (Since
   w8-editor-smalls-z: `editor.matgraph_find`, the non-mutating twin of the canvas node search
   `editor.matgraph_search` — same listing, no `add` — so a read-only agent can list results;
   rev-claude-47-k finding 3.)

Tests: a collapse (inline and to a file) leaves the normalized WGSL unchanged and expand restores it; a
signature change marks both users stale and the fix rewires one; an instance override written from the canvas
equals the instance API's bytes and the parent is never written; undo restores the graph, instance and
function-file bytes; undo of a parked tab's step edits that tab.

## 12. Stage 2: preview shapes, typed inspector, canvas productivity, per-node previews, level hookup (ADR-9700)

Added for v0.6.0 Stage 2 (2026-11-12), editor only (no `.matgraph.ron` / `.matfunc.ron` change):

1. **Preview shapes.** The graph preview and the Material panel draw on a sphere, a cube, a plane or a project
   `.glb` / `.gltf` (its first part, centered and scaled to the frame, at most 250 000 vertices); dragging the image
   orbits the camera and a slider turns the light. A mesh that fails to load keeps the last image and says why.
2. **Typed inspector.** The selected node's values are typed rows (`inspector::fields` / `set_field`): drag values
   and a color picker for constants, dropdowns for names and enums, the texture picker with a thumbnail, the
   parameter's default with its type's widget, `Noise` octaves clamped to 1..8, a function picker for `Call`. Each
   change is one edit; a drag coalesces. RON stays as an Advanced row.
3. **Canvas productivity.** Fuzzy node search (Space or Tab on the canvas); a wire dropped into empty space opens it
   filtered to the kinds that can take the wire and wires the new node. Copy, cut and paste within and across tabs
   (RON of the nodes and relative positions; clashing names renamed, references follow, references outside the
   copy unwired). Shift+drag box select; align and distribute. Comment frames and reroute points in the layout
   sidecar v2 (§ 2.8).
4. **Per-node previews.** A toggle shows a swatch per node: the node routed to an unlit base color in a derived
   graph (fast compile path), rendered only when the revision changes, capped per frame, cached by pipeline key. As built (ADR-7451): the
   cache holds at most `NODE_PREVIEW_SWATCH_CAP` (256) swatches, least recently used out first.
5. **Level hookup.** Apply to selection assigns the open graph or instance to the selected meshes'
   `MeshSource.material` (the content browser's drop edit, one undo step; as built, ADR-7451: all owners are
   captured before any changes, so a refused owner leaves every owner unchanged and no undo step); the Content Browser's New menu makes
   material graphs and functions.
6. **Agent.** `editor.matgraph_preview { shape, mesh?, yaw?, pitch?, light_yaw? }`, `editor.material_preview`,
   `editor.matgraph_node_fields`, `editor.matgraph_node_set`, `editor.matgraph_search`, `editor.matgraph_clipboard`,
   `editor.matgraph_arrange`, `editor.matgraph_annotate`, `editor.matgraph_node_preview`,
   `editor.matgraph_apply_to_selection`, `editor.create_asset { kind: "material_graph" | "material_function" }`;
   codes `matgraph_preview_mesh_failed`, `matgraph_node_missing`, `matgraph_apply_refused`, `material_preview_failed`.

Tests (`matgraph_editor/s2_tests.rs`, the panel test): a red base color reads red with coverage on the cube, the
plane and a project mesh (lavapipe; skips with no adapter); the typed rows and the RON path give identical nodes for
every kind in `Node::KINDS`; a 3-node selection pasted into a second tab compiles to the same normalized WGSL; a v1
layout loads under v2 and v2 round-trips byte for byte; a `Noise` node's swatch has many gray levels and a
constant's few; undo restores the graph and layout bytes for paste, cut, a typed field, align, a comment, a reroute
and a search-add; a headless panel test drives open, search-add, a typed edit, copy and paste, apply to selection
and save.

### 12.1 Canvas finish (ADR-9970, v0.6.0 Stage 2 follow-up)

As built (2026-11-12), editor only, layout sidecar still v2 (no new field):

1. **Rerouted wires.** An input with reroute points that is wired to an existing node gets no egui-snarl wire; the
   canvas draws it as one curve from the output pin through the points, in order, to the input pin
   (`matgraph_editor::canvas::reroute_path`, a uniform Catmull-Rom curve with level phantom ends). egui-snarl 0.12 has
   no wire hook, so the pins record where snarl placed them and the curve is painted into a shape slot reserved
   behind the nodes. Points drag on the canvas (one undo step per drag); double-click adds a point after one,
   right-click removes one. Agent twin: `editor.matgraph_annotate { op: "reroute_set" }`.
2. **Comment frames.** The header strip (28 canvas points) moves the frame and the nodes inside it (a node is inside
   when its position, its top-left corner, lies in the frame at the start of the drag); edges and corners resize it
   (at least 40 points a side); double-click edits the text. One undo step per drag. The side list stays. Agent twin:
   `comment_set { index, rect | min + max, move_nodes? }` (answers `moved`).
3. **Material panel preview.** Rendered only when (material value, shape and mesh path, yaw, pitch, light turn,
   size) changes; a failed render waits for one of those to change too.

*Superseded by § 12.2 (ADR-7730): wire clicks are built.* Not built: adding a reroute point by clicking a plain wire (snarl owns wire hits; points come from
`reroute_set` or by double-clicking a point), disconnecting a rerouted wire by right-clicking the wire (right-click the input
pin's menu or Clear in the side list).

### 12.2 Gaps closed (ADR-7730, w11)

As built (2026-11-14), editor only except item 3, no format change:

1. **Live function tabs.** A graph or instance compiles a function file that is open in a function tab from the
   tab's definition, saved or not (diagnostics, compile, preview, swatches, policy count); a function tab is checked
   against its calling graph's open tab as it is now, else the graph read when it opened (§ 11.2's snapshot). A
   function tab previews its calling graph with its definition (else its first output, unlit).
2. **Wire clicks.** The canvas draws plain wires as cubic Béziers it can reconstruct (snarl `Bezier3`, a fixed frame)
   and hit-tests them by subdivision, and rerouted wires as their polylines: a click adds a reroute point, a
   right-click on a rerouted wire disconnects it. This replaces § 12.1's "Not built" paragraph.
3. **Shared expansions.** `regia_matgraph`'s lowering gives calls of one function with the same arguments one
   expansion, so a multi-output collapse equals the code before (§ 11.4's single-output rule now holds for any).
4. **WGSL view and Function users** in the side column, the panel twins of `editor.matgraph_compile { include_wgsl }`
   and `editor.matgraph_function_users`.
5. **Visible swatches** are pinned: § 12.4's cap evicts only swatches of off-screen nodes.
