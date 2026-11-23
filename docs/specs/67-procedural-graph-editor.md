# Spec 67: Procedural graph editor and interactive tools

Status: proposed (ADR-1596). Part of the procedural content graph (spec 62). Behavior written from the public-sources spec
set, area `runtime-procedural` (ADR-1526, behavior only); panel layout, shortcuts, tool kinds and names are Regia's own.
Desktop only (`regia_editor`, layer 6).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built (ADR-5646 audit, 2026-11-10): `regia_editor::procgen` (document, undo, panel, preview, visualize, inspect, generator
inspector, scripted placement tools and every § 6 method; ADR-1675, ADR-2422, ADR-2786, ADR-2907). Since built (w11 audit,
2026-11-15, ADR-7743): the interactive viewport tool mode of § 5 (`procgen::tools::viewport_input`, called from
`viewport.rs`; Arm / Cancel in the panel's tool row), the panel's preview bar (`procgen::panel::preview_bar`), the
Inspect cell picker and row hover (`procgen::panel::table_controls` and `table_ui`), the Parameters tab beyond
add/remove (`procgen::params_tab`, § 2.1 below), and the presets `tool_fence`, `tool_rubble_box` and
`tool_painted_props` (`procgen::presets`, § 5.4). Not built: nothing of this header list. The patterns it reuses are built: `regia_editor::anim` (graph panel),
`regia_editor::command` (`EditorCommand`: `apply`, `revert`, `merge`, `description`; `UndoStack`), `regia_editor::terrain_tool`
(a viewport tool resource with `viewport_input` and `frame_effects` hooks called from `viewport.rs` and `shell.rs`),
`egui-snarl 0.12`, `shell::Panel` (add a `ProcGraph` variant).

## 1. Purpose

Authors (people and agents) need to build and debug procedural graphs where they see the result: a node canvas next to
the viewport, live regeneration of a bound generator, per-node visualization and data tables, and viewport tools that draw
paths, areas, painted points and boxes that graphs consume. Everything the panel does is also an agent method, and every
edit is undoable with the level's other edits.

Baseline reused (follow these patterns, do not invent new ones):
- The anim graph editor (`regia_editor::anim`): document state in a world resource, `egui-snarl` canvas, panels and the
  agent editing the same document, every edit a snapshot command on the shared `UndoStack`, capture of panels for tests.
- `regia_editor::command` (`EditorCommand`, `UndoStack` with coalescing, 500 entries).
- The terrain tool (`regia_editor::terrain_tool`): viewport tool mode, strokes on a background thread, one undo entry per
  stroke, Shift inverts.
- Editor agent extension methods (`editor.*`, `regia_editor::agent_ext`), panel capture (`ui_capture`), atomic file
  writes (`level_io`).

## 2. The graph panel (`Panel::ProcGraph`, module `regia_editor::procgen`)

Module layout: `procgen/doc.rs` (`ProcGraphDoc` world resource: the open `ProcGraph` from spec 62 `regia_procgen::graph`,
dirty flag, selection, preview target, visualize set; every mutation is a `ProcGraphCommand` implementing
`EditorCommand`, holding a before/after snapshot of the touched nodes, edges or parameters), `panel.rs` (egui, reads the doc),
`debug_view.rs` (visualize and inspect), `tools.rs` (§ 5), `agent.rs` (§ 6). Facts used from other specs: a graph holds
nodes (`id`, `ty`, `settings`, `enabled`), edges, parameters, notes and `flags { starter, tool, domain }`; pins have a
`DataKind` (see § 2.2); settings may be `Bind(param)` and may expose an override pin `@<setting>`; compile errors are a list
of `{ node?, code, message }`. The editor links `regia_procgen` with the `pipeline` feature.

### 2.1 Layout

A dockable panel with:
- **Canvas** (`egui-snarl`): nodes with their pins colored by data kind (§ 2.2), edges, notes and reroutes. Pan, zoom,
  box select, copy/paste (as RON fragments on the clipboard), duplicate, align, delete.
- **Palette** (left, collapsible): node types from the registry (`procgen.node_types`), grouped by category, with search
  over id, category and the one-line doc. Dragging from an output pin onto empty canvas opens the palette filtered to node
  types with a compatible input pin; dropping connects it.
- **Node settings** (right): the selected node's settings as typed widgets generated from the node schema (numbers with
  ranges and drag, vectors, enums, curves with a small curve editor, asset pickers, name lists). Each setting has a "bind to
  parameter" toggle (creates or picks a graph parameter, writes `Bind(...)`) and a "show as pin" toggle (adds the override
  pin `@<setting>`). Invalid values are shown in red with the validation message and are not written.
- **Parameters** (tab beside settings): declare, rename (updates every `Bind` in the graph in one undo entry), reorder,
  set type, default, range and doc.
- **Run report** (bottom strip): state of the bound generator, run time, per-node time bars, counts, cache hits; inactive
  nodes (spec 62 § 5.1) drawn dimmed; errors shown as a red badge on the failing node with the code and message.
- **Data table** (bottom tab): rows of the inspected node's output (§ 3).

### 2.2 Pin colors and badges

One color per `DataKind` (Points, Table, Spline, Polygon, Shape, Ground, Mask, Mesh, PlantSkeleton, PlantMesh),
multi-kind pins striped. Badges on a
node: bypassed (gray stripe), visualized (eye), inspected (table), tier (spec 66, the tier's cell size), cache hit (small
dot), warning count, error. Colors come from the editor theme (`regia_editor::theme`) with a colorblind-safe default set.

### 2.3 Documents and saving

- Opening a `.pgraph.ron` edits it; opening a `.pgvar.ron` shows the parent graph read-only with an override column in the
  Parameters tab (editing a value writes the variant's override; "reset" removes it).
- Save is an atomic write. An outside change to the open file (hot reload) with no unsaved edits reloads silently; with
  unsaved edits it shows a choice (keep mine / take theirs), never a silent overwrite.
- **New graph**: from the content browser ("New procedural graph"), with a picker of starter graphs (graphs whose
  `flags.starter` is true, spec 62 § 4) or a blank graph with `in.self` and an output node. The picker can be disabled in
  editor preferences.
- **Make starter**: a graph's "starter" checkbox in the graph properties.

### 2.4 Editing operations (each one undo entry)

Add node, remove nodes (with their edges), connect, disconnect, move (coalesced), set setting (slider drags coalesce),
bypass toggle, add/remove/rename parameter, bind/unbind setting, show/hide override pin, add note, add reroute, paste.
Also:
- **Collapse to subgraph**: selected nodes move into a new graph file; edges crossing the selection become the new graph's
  input and output pins; a `graph.call` node replaces them. The generator's output hash must be identical before and after
  (test).
- **Expand subgraph**: the inverse for a `graph.call` whose target is used only here (otherwise it asks to copy).
- **Promote to parameter**: from a setting, as the bind toggle with a name prompt.
- **Open called graph**: double-click a `graph.call`/`graph.loop` node; a breadcrumb row shows the call path and returns.

## 3. Live preview and debugging

- **Preview target**: the panel binds to a generator in the level ("Preview on" picker; defaults to the selected entity's
  generator, else the last one used). Edits recompile at once; a valid graph re-runs the target after a quiet period
  (default 150 ms, editor preference `procgen_refresh_ms`), cancelling an in-flight run (spec 62 § 7.2). Invalid graphs do
  not run; the last good output stays.
- **Debug data**: preview runs set `keep_debug: true` (a `RunInputs` flag of spec 62), which keeps every node's output bundles for the last run (capped at
  256 MiB per generator, oldest nodes dropped first, the report marks dropped nodes). Runtime and bake runs never keep
  debug data.
- **Visualize** (`V` on the selected nodes, or the eye badge): the node's output is drawn in the viewport: points as dots,
  boxes (their oriented bounds) or axes; colored by density (black to white), by a chosen attribute (scalar ramp or
  categorical colors for names), or by tags. Splines draw as curves with control points; polygons as outlines; shapes as
  wire volumes; masks as a translucent ground overlay. Several nodes may be visualized at once, each with its own color.
  `debug.show` nodes (spec 63 § 12) are always visualized in preview.
- **Inspect** (`I`): fills the Data table with the node's output: one tab per output pin, an item picker, columns for
  built-ins and attributes (sortable, filterable by expression `attr op value`), paging by 1 000 rows, row hover highlights
  the element in the viewport and clicking a point in the viewport selects its row. For tiled generators a cell picker
  (tier, i, j) chooses which cell's data to show.
- **Bypass** (`B`): toggles `enabled` (spec 62 § 4).
- **Isolate**: "preview up to here" runs the graph only up to the selected node (its ancestors) and visualizes it; the
  generator output is not replaced while isolating.
- **Seed**: the preview bar shows the generator seed with "pin", "new seed" and "same seed as …" actions.
- **Quality tier**: a preview tier selector feeds `flow.quality` (spec 63 § 10).
- **Generate around view**: a toggle that makes the editor camera an anchor for `Anchored` generators (spec 66 § 4.1).
- **Tiles overlay**: toggles the spec 66 debug view.

## 4. Generator inspector

Selecting an entity with `ProcGenerator` shows, in the regular inspector, the component fields plus: the graph's
parameters with override column (edit = override on this generator, reset = remove), Run / Cancel / Clear / Detach / Bake
buttons, the run state and counts, stale indicator with the reason (which dependency changed), and an "Open graph" button.

## 5. Placement tools (viewport tool mode "Procedural")

A placement tool turns viewport gestures into the *inputs* a graph reads (a spline, a box shape, painted points) and keeps
a generator bound to them. The generator's graph is the tool's **preset**: any graph whose `flags.tool` (spec 62 § 4, a
`ToolKind` value or none) equals the active tool kind. Choosing a preset in the tool panel is the same as assigning that
graph to the generator by hand, and the preset's exposed parameters edit the generator's overrides.

### 5.1 Session model

```rust
pub enum ToolKind { Path, Area, Paint, Box }
pub struct ToolSession { // world resource `ProcToolSession`, at most one open session
    pub kind: ToolKind, pub preset: String /* graph path */, pub target: Entity,
    pub created: bool,                     // the session spawned `target`
    pub undo_snapshot: Vec<ComponentSnapshot>, // components the session edits, to restore on cancel
    pub hit: HitFilter,
}
```

1. **Arm:** pick a kind, then a preset. Presets whose kind or domain does not fit the current target are not listed.
2. **Resolve the target** at the first gesture. If the selected entity already carries what the kind needs (`Spline` for
   Path, a closed `Spline` for Area, `PaintedPoints` for Paint, a box `Shape` for Box) the tool edits it, adding a
   `ProcGenerator` bound to the preset when it has none. Otherwise it spawns a new entity named after the preset
   (`<preset>_01`, counting up) with the right component and generator. With the preference "edit selected only" on, an
   unfit selection leaves the tool inert and the tool button shows why (`procgen_tool_target_unfit` for the agent).
3. **Edit:** gestures change the target's data (table below). The generator re-runs while the session is open, at most
   `tool_refresh_hz` times per second (default 10, an editor preference; tests count editor ticks instead).
4. **Finish:** Apply commits everything the session did as one `EditorCommand` (one undo entry). Cancel despawns what the
   session spawned and restores `undo_snapshot`, leaving the level byte-identical.

### 5.2 Gestures

| Kind | Edits | Gestures |
|---|---|---|
| Path | `Spline` (open by default; spec 64 § 2: `points`, `closed`, tangent modes, roll) | click appends a point on the surface under the cursor; drag a point to move it; Backspace drops the last point; `C` toggles closed; Enter applies; Esc cancels |
| Area | closed `Spline`; also a `BiomeArea` (spec 65: `biome`, `shape: FromSpline`, `blend_m`) when the preset's graph is a biome graph, else a `ProcGenerator` | same as Path, but the outline is closed on apply |
| Paint | `PaintedPoints` | brush strokes stamp points on the surface (radius, spacing, density, random yaw); Shift erases points under the brush; `[` and `]` resize it; digits 1 to 8 choose the layer |
| Box | box `Shape` (half extents, entity transform); as built the generator's own `ProcBounds::Box` and transform (ADR-1675 (10); switching to `Shape` + `FromShape` is a v0.5.x item) | drag a footprint rectangle on the ground, then drag vertically for the height |

`PaintedPoints` is a new saved component (`format_version` 1, check-formats name `procgen-painted-points`): `points: Vec<
PaintedPoint { position: Vec3, normal: Vec3, layer: u8 }>` plus `layer_names: Vec<String>` (at most 8). `in.self`
(spec 63 § 3) reads it as a point set tagged `painted` with the `layer` attribute (an integer) filled in, so a graph filters
layers with an ordinary attribute filter. Strokes run on a background thread like the terrain tool's, one undo entry per
session.

### 5.3 Where gestures land

A `HitFilter` (saved per tool in editor preferences) decides what the cursor ray may hit: `terrain: bool` (default true),
`meshes: bool` (default true), `skip_generated: bool` (default true; hits on entities carrying `Generated` are ignored so
a tool never builds on its own output), `only_entity: Option<Entity>` (restrict to the selection) and `physics_mask: u32`
(default all layers). The ray cast uses the editor's pick path (`regia_editor::picking`, terrain through the terrain query).
A cursor ray with no hit places nothing; the agent reports `procgen_tool_no_hit`.

### 5.4 Shipped tool graphs

Starters live under `starter/` and carry the tool flag: `tool_fence.pgraph.ron` (Path, modules along a spline),
`tool_road.pgraph.ron` (Path, the spec 64 road network), `tool_forest_area.pgraph.ron` (Area, a biome layer),
`tool_rubble_box.pgraph.ron` (Box), `tool_painted_props.pgraph.ron` (Paint). As built (ADR-2422 (2)) `tool_road` and
`tool_forest_area` ship, embedded in `regia_editor::procgen::presets` and written into the project on first use; the other
three are not shipped yet.

## 6. Agent protocol surface (editor extension methods, page `book/agents/procgen-editor.md`)

| Method | Class | Params | Result |
|---|---|---|---|
| `editor.procgen_open` | read | `{ path }` | opens the panel on the document; `{ graph_hash, errors }` |
| `editor.procgen_new` | assets | `{ path, starter? }` | creates the file (fails if it exists: `procgen_file_exists`) |
| `editor.procgen_preview` | edit_scene | `{ generator }` | binds the preview target |
| `editor.procgen_select` | read | `{ nodes: [id] }` | selection set (for capture and inspection) |
| `editor.procgen_visualize` | read | `{ node, on, color_by?: density\|attr\|tags, attr? }` | `{ visualized: [id] }` |
| `editor.procgen_isolate` | read | `{ node \| null }` | runs up to the node; `{ state }` |
| `editor.procgen_collapse` | assets | `{ nodes: [id], path }` | `{ call_node, new_graph }` |
| `editor.procgen_expand` | assets | `{ node }` | `{ nodes }` |
| `editor.procgen_capture` | read without `out`; edit_scene with `out` (as built, ADR-5646: the method is registered `read` and its handler refuses `out` with `F0012` without `edit_scene`; the mutating rate limit is not applied) | `{ panel: canvas\|table\|report, out? }` | PNG of the panel via the existing UI capture (heavy). Without `out` the PNG is returned base64 in the result; with `out` it is written to a path confined to the same roots as `render.out_start` (protocol Round 2), which needs `edit_scene` and counts against the mutating rate limit |
| `editor.procgen_tool` | edit_scene | `{ tool: path\|area\|paint\|box, preset, target?, points?: [[x,y,z]...], closed?, move?: { index, to }, remove?: index, clear?, brush?: { radius_m, spacing_m, erase, layer }, box?: { min, max }, apply: bool }` | scripted tool session: points are projected with the tool's hit filter; returns the entity, generator state and counts; `apply: false` leaves the session open for more calls |
| `editor.procgen_tool_cancel` | edit_scene | `{}` | ends the open tool session, removing what it created; `{ cancelled: bool }` (false when no session was open) |

Graph content edits through the agent use `procgen.graph_edit` (spec 62 § 9); when the graph is open in the panel, those
edits land in the panel's document and undo stack instead of writing the file directly, and `save: true` saves the panel's
document. Errors: `procgen_file_exists`, `procgen_not_open`, `procgen_no_preview`, `procgen_tool_target_unfit` (with the
reason), `procgen_tool_no_hit` (a point did not hit anything under the filter), `procgen_collapse_invalid` (selection
with no single connected boundary or containing graph inputs/outputs), plus `F0012` when the editor is read-only.

`book/COVERAGE.md` rows for the `editor.procgen_*` methods.

Formats: no existing format changes. `PaintedPoints` is new at `format_version: 1` (no migrator needed). Editor
preferences gain `procgen_refresh_ms`, `tool_refresh_hz` and per-tool `HitFilter`s (preferences are not a frozen format).
The lead runs `cargo xtask check-formats --update` and `cargo xtask manual`.

## 7. Tests and acceptance

Model and panel tests (`regia_editor`, headless; no GPU except where the existing capture tests already use lavapipe):
1. Every editing operation applies, undoes and redoes to byte-identical files (round trip through `UndoStack`), and
   slider drags coalesce into one entry.
2. Rename parameter updates every `Bind` and is one undo entry.
3. Collapse to subgraph then expand gives the original graph hash; the bound generator's output hash is identical after
   collapse (the run is recomputed).
4. Agent `procgen.graph_edit` on an open document lands in the undo stack (one `editor.undo` reverts it).
5. Hot reload: an outside change with no unsaved edits reloads; with unsaved edits, the choice prompt state is set and the
   file is not overwritten.
6. Preview: a settings change re-runs the generator once after the quiet period (counted in editor ticks with a test
   clock, no sleeps); three quick changes cause one run (debounce) and the earlier run is cancelled.
7. Inspect: the data table shows the node's rows; sorting by density is stable; clicking a row selects the point (selection
   index equals row index).
8. Visualize: a visualized node registers its gizmo draws (count equals visible points under the cap of 200 000 drawn
   points, beyond which it draws a uniform subsample and says so).
9. Tools via agent (as built with the `tool_road` preset, since `tool_fence` does not ship yet; ADR-2422):
   `editor.procgen_tool { tool: path, preset: fence, points: [...4 points...], apply: true }` creates one
   entity with a 4-point `Spline` and a ready generator; undo removes the entity; Cancel leaves the level unchanged
   (scene hash equal); Paint with `erase: true` removes the points under the brush; Box creates a shape whose extents match
   the given min/max.
10. Unfit target: Path tool with an entity lacking a spline selected and "edit selected only" on returns
    `procgen_tool_target_unfit`.
11. Hit filter: with `skip_generated` on, a ray that crosses a `Generated` entity first hits the surface behind it; with
    it off, it hits the generated entity. A Paint stroke on layer 3 writes `layer == 2` (zero based) points that an
    attribute filter in a test graph selects exactly.
12. Panel capture of the canvas for the acceptance graph is produced (non-empty PNG, expected size); no golden image
    (lead TODO if one is wanted).

Acceptance (owner-visible): build the "rocks on slopes" graph from a blank document entirely through the agent
(`editor.procgen_new`, `procgen.graph_edit`, `editor.procgen_preview`), visualize the filter node, inspect its table,
then draw a fence with `editor.procgen_tool`, and screenshot the viewport and the canvas for review.

## 8. Platforms

Editor only: Linux, Windows, macOS (testing). Nothing in `regia_editor::procgen` is linked into games. Android has no
editor; the runtime parts it needs are in `regia_procgen` (spec 62).

## 9. Performance

Panel frame cost ≤ 2 ms with a 300-node graph on the desktop reference (`tracing` span `editor.procgen_panel`); visualizing
200 000 points ≤ 3 ms GPU on the reference card through the gizmo instanced path; data table paging ≤ 1 ms per page.

## 10. Out of scope

Visual scripting of game logic (parked, Rust only); a shader/GPU-node authoring surface; multi-user editing of graphs; a node-level profiler beyond the run
report; diffs/merges of graph files beyond text (RON stays diff-friendly: maps sorted by node id, one edge per line).

## 11. Provenance

Behavior of procedural graph editors and interactive procedural tools in public user documentation, via the public-sources
spec set area `runtime-procedural` (set spec ids `runtime-procedural-graph-authoring`, `-overview`; their Sources pages;
no menu text, shortcuts, panel names or wording carried). Editor patterns follow Regia's own anim graph editor and terrain
tool (their own ADRs and provenance rows record where those designs came from). Shortcuts, tool set, debounce and caps are Regia design choices.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 12. Owner decisions

None.

## Addendum (ADR-7215): as built after v0.5.0

- **§ 5 tool mode.** The viewport mode "Procedural" takes Path and Area (click, drag, Backspace, `C`, Enter,
  Esc; ADR-3480), Paint (drag strokes, Shift erases, `[`/`]` radius, digits layer) and Box (footprint drag,
  then height drag). Every gesture calls `run_tool`/`finish`; agents use `editor.procgen_tool { mode, gesture }`.
- **§ 5.2 outputs.** Area on a biome preset (a `biome.areas` graph) writes `BiomeArea { biome, shape:
  FromSpline }` instead of a generator; the biome is the call's `biome` or the preset's `biome` parameter. Box
  writes a box `Shape` and binds the generator with `ProcBounds::FromShape` (the ADR-1675 (10) interim is
  retired; `ProcBounds::Box` stays in the format, so no migrator). Paint stamps a seeded random yaw per point
  (`procgen-painted-points` v2, `yaw`), and strokes are stamped in the background over editor ticks (a
  2048-cell budget per tick) on the editor thread, because the stamping reads the level through the pick path.
- **§ 3 preview bar.** Preview on (follow selection / none / a generator), tier, isolate, generate around view,
  tiles, seed field, pin, new seed, same seed as. The default target is the selection's generator, else the last
  used, until a generator is chosen (`editor.procgen_preview { follow }`).
- **§ 5.3.** Hit filters are saved per tool in the editor preferences (`procgen.hit.<tool>.*`).
- **§ 5.4.** All five tool presets ship (`tool_biome_area` stands in for a biome-layer forest area).


## Addendum (ADR-7239): the graph panel remainder, as built after v0.5.0

- **§ 2.1 Parameters tab.** Declare (any type, zero default), rename (every `Bind` follows), reorder, type (the
  default converts when it can), default, range (number parameters) and doc, one undo entry each;
  `editor.procgen_param` takes the same fields (`declare`, `rename`, `index`, `ty`, `min`, `max`, `doc`) in one entry.
- **§ 2.1 typed widgets.** Numbers, toggles, vectors, ranges, enums, a small curve editor, an asset picker (project
  files) and name lists; RON text only for `Any`. Values are checked before they are written and a refusal shows in red
  under the widget. `editor.procgen_widgets` reads the widgets and checks a value without writing.
- **§ 3 data table.** Pin tabs, item picker, filter field, pager, header-click sort and row select in the panel;
  `editor.procgen_table` without `node` reads the panel's view, `toggle_sort` is a header click, and a refused filter
  keeps the last good one.
- **§ 2.2.** Pin colors are theme tokens (`data_kinds`); multi-kind pins are striped; badges for bypassed, visualized,
  inspected, tier, cache hit, warning count and error (`editor.procgen_badges`). A tier-neutral node shows a tier
  badge only in a single-tier graph; since ADR-7275 it names every tier it runs in (`T0+T1`; `tier.shared`,
  `tier.feeds`).
- **§ 2.4.** Promote opens a name prompt (suggested free name, red when invalid) or binds to an existing parameter of the
  setting's type (`editor.procgen_promote`).
- **§ 6.** `procgen_collapse_invalid` also covers a selection that is not one connected group or that a path leaves
  and re-enters; the node menu disables Collapse with the reason.
- **§ 5.4.** `tool_fence` ships (six presets).
- **§ 2.2 (ADR-7743).** A tier-neutral node's `tier` row has `neutral: true`; one that feeds a single tier shows `~`
  before that tier's cell size (`~64 m`) so it does not read as a node placed at that tier.
