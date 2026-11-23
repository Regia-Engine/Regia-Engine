# Procedural graph editor: canvas, preview, data tables, placement tools

The editor's **Procedural Graph** panel edits a `.pgraph.ron` next to the viewport (specs/67). A
canvas shows the nodes with their pins colored by data kind; a palette lists the node types; the
right side edits the selected node's settings and the graph's parameters; the bottom strip shows the
bound generator's run report and the data table of an inspected node. Every edit is one entry on the
editor's undo stack, so `editor.undo` (or Ctrl+Z) reverts graph edits in order with level edits.

Agents use the same document: `editor.procgen_*` methods (below) and the runtime methods of
[Procedural generators](procgen.md). When a graph is open in the panel, `procgen.graph_edit` on that
path lands in the panel's document and undo stack instead of the overlay (its result has
`"document": true`), and `save: true` saves the panel's document.

The editor installs `ProcGenPlugin` with editor staleness rules and reads graphs from the open
project. Everything here is desktop only; nothing is linked into games.

## Open, create and save

```json
editor.procgen_new { "path": "procgen/rocks.pgraph.ron" }
→ { "path": "procgen/rocks.pgraph.ron", "graph_hash": "…", "errors": [], "variant": null }
editor.procgen_open { "path": "procgen/rocks.pgraph.ron" }
```

`editor.procgen_starters {}` lists the picker: the project's starter graphs `{ path, tool, domain, description }`
(empty with `"enabled": false` when the picker is off in editor preferences); `editor.procgen_starters { "starter":
true }` is the graph properties' **Make starter** checkbox (one undo entry; the file lists once saved). The panel's
**New…** menu takes a path and a starter. A new graph copies a starter graph (`starter`: a graph whose `flags.starter`
is true; the copy is not a starter) or starts blank: `in.self` (1) → `sample.region` (2) → `out.points` (3), valid as is. The
file must not exist (`procgen_file_exists`). Opening a `.pgvar.ron` shows its parent graph read-only:
graph edits answer `procgen_read_only`; the Parameters tab edits the variant's overrides.
`procgen.graph_edit` on the open variant path or on its parent path returns that reason before it
loads or writes either file, and it leaves the overlay unchanged.

Saving is an atomic write (`procgen.graph_edit { …, "save": true }` or the panel's Save button). An
outside change to the open file (another tool, version control) reloads silently when the document has
no unsaved edits. With unsaved edits the panel asks *keep mine* or *take theirs* and never overwrites
the file on its own; agents see `"conflict": true` in `editor.procgen_state` and answer with
`editor.procgen_reload { "take_theirs": true | false }`. `editor.procgen_reload {}` checks the file now
(the panel checks every 30 editor frames).

The Parameters tab's values go through `editor.procgen_param { "name": "spacing", "value": 6.0 }`: on an open
variant it writes the variant's override (`"reset": true` removes it), on a graph it sets the declared default; one
undo entry, consecutive changes of one parameter coalesce (`procgen_param_value` when the value is missing or a graph
default is reset).

**The Parameters tab** (beside Node in the settings panel, spec 67 § 2.1) declares, renames, reorders and types the
graph's parameters. Each row has the name (typed, renamed on Enter or focus loss; red while it is not a new valid name),
▲ and ▼ to move it, the type, the default as a typed field, min and max for number parameters, the doc line and
remove; the last row declares a new parameter of the chosen type with that type's zero default. Every control is one
undo entry; renaming rewrites every `Bind` of the parameter in the same entry. A type change keeps a default that
converts (a number to a number, rounding to an integer; a name to an asset path; a vector's leading components) and
otherwise sets the new type's zero; a type without one value per component drops the range. On an open variant the
declarations are the parent's (read-only) and the value column edits the variant's override, with **reset**.

The same edits through the agent, in one undo entry per call (applied in the order declare or type, value, range,
doc, index, rename):

```json
editor.procgen_param { "name": "spacing", "rename": "cell" }
editor.procgen_param { "name": "mask", "index": 0 }
editor.procgen_param { "name": "count", "ty": "Float", "min": 1.0, "max": 9.0, "doc": "How many" }
editor.procgen_param { "name": "offset", "declare": true, "ty": "Vec3" }
editor.procgen_param { "name": "mask", "remove": true }
→ { "name": "cell", "overrides": {}, "dirty": true,
    "params": [{ "name", "ty", "default", "min", "max", "doc" }, …] }
```

`min` or `max` `null` clears that bound. `remove: true` is the row's **remove**: it drops the declaration and every
setting bound to the parameter takes its default, in one undo entry; it takes no other edit key (`procgen_param_value`)
and an open variant refuses it (`procgen_read_only`). Errors: `procgen_param_name` (the new name is invalid or taken),
`procgen_unknown_param`, `procgen_param_type` (an unknown type, or a value that is not of the parameter's type),
`procgen_param_range` (min above max, a range on a parameter that is not a number, or a bound that is not finite).

`editor.procgen_state {}` returns the document (`path`, `dirty`, `read_only`, `conflict`, `selection`,
compile `errors`, `revision`, `breadcrumb`), the preview (`generator`, its `state`, `run_id`,
`output_hash`, `runs`, `cancelled`, `pending_ticks`), the visualized nodes and draw counts
(`drawn_points`, `total_points`, `subsampled`), `dropped_debug`, `isolated`, `undo_depth` and the open
tool session.

## Edit the graph

Graph content changes go through `procgen.graph_edit` (ops `add_node`, `remove_node`, `connect`,
`disconnect`, `move`, `set_setting`, `set_enabled`, `add_param`, `rename_param`, …; see
[Procedural generators](procgen.md#edit-a-graph)). Each call is one undo entry.

The panel adds, per spec: palette search over id, category and doc line; right-click the canvas to add
a node, a note or a reroute; node menu Bypass (`B`), Visualize (`V`), Inspect (`I`), Duplicate (Ctrl+D), Delete,
Collapse (the nodes of the selection when it contains the node, otherwise that node, written to the path typed in
the menu), Expand and Expand as copy on a `graph.call`, and Isolate; box select on the canvas selects
nodes; Ctrl+C copies the selection as a RON graph fragment (system clipboard) and Ctrl+V pastes one with fresh ids;
settings as typed widgets (below); **bind…** promotes a setting to a graph parameter (`Bind("name")`, below),
**unbind** writes the parameter's default back. Renaming a parameter updates every
`Bind` in one undo entry; removing one writes its default into the settings bound to it. Node drags and
slider scrubs coalesce into one entry until the pointer is released.

`editor.procgen_select { "nodes": [2, 3] }` sets the canvas selection.

**Typed setting widgets.** The Node tab draws each setting with the widget of its schema type: numbers drag inside the
schema's range (integers in whole steps), booleans are checkboxes, `Vec3` three drag fields, `[lo, hi]` ranges two,
vector ranges two rows of three, enums a list, **curves** a small curve editor (drag a key, double-click empty space to
add one, right-click a key to remove it; keys stay sorted by x and a curve keeps at least two), **assets** a path field
with a picker that lists project files matching a search, **name lists** one field per name with × and +, and RON text
for the types a node checks itself. Every value is checked against the compiler before it is written: a refused value
is shown in red under its widget with the code and message and is not written. Agents read the widgets and check a
value without writing it:

```json
editor.procgen_widgets { "node": 3, "search": "mask", "check": { "setting": "jitter", "value": 4.0 } }
→ { "node": 3, "ty": "sample.region",
    "settings": [{ "name": "spacing_m", "widget": "number", "value": "Bind(\"spacing\")", "bound": "spacing",
                   "min": 0.01, "max": 1000000.0, "doc": "…" },
                 { "name": "height", "widget": "enum", "options": ["region", "value"], … }, …],
    "check": { "ok": false, "reason": "setting_out_of_range", "message": "…" } }
```

`widget` is `number`, `integer`, `toggle`, `vec3`, `range2`, `vec3_range`, `enum`, `curve` (with `points`: the
`[x, y]` keys), `asset` (with `choices`: project paths containing `search`, at most 200), `name`, `name_list` (with
`items`) or `text`. Values are written with `procgen.graph_edit` `set_setting`.

**Promote to parameter** (the **bind…** menu next to a setting) opens a name prompt that starts at a free name (the
setting's, else `<setting>_2`, …) and is red while the name is invalid or taken; Promote declares the parameter, typed
after the setting with its current value as default, and binds the setting (one undo entry). The same menu lists the
graph's parameters of that type to bind to instead. Curves, lists and ranges cannot be parameters.

```json
editor.procgen_promote { "node": 2, "setting": "jitter", "name": "wiggle" }
→ { "param": "wiggle", "created": true, "node": 2, "setting": "jitter" }
editor.procgen_promote { "node": 2, "setting": "y", "name": "spacing", "existing": true }
→ { "param": "spacing", "created": false, … }
```

Without `name` the suggested name is used. Errors: `procgen_param_name` (invalid or taken), `procgen_unknown_param`
(`existing` names no parameter), `procgen_param_type` (the parameter has another type, or the setting cannot be a
parameter), `unknown_setting`, `procgen_unknown_node`, `procgen_read_only`.

**Pin colors and badges** (spec 67 § 2.2). Each pin takes its data kind's color from the editor theme
(`ThemeTokens::data_kinds`, JSON key `data_kinds` with keys `points`, `table`, `spline`, `polygon`, `shape`, `ground`,
`mask`, `mesh`, `plant_skeleton`, `plant_mesh`; the default set is colorblind-safe). A pin that takes several kinds is
a square striped with one color per kind; a pin of every kind is one neutral gray. After a node's title: a gray stripe
when bypassed, `eye` when visualized, `table` when inspected, the tier's cell size (`64 m`, or `whole`) when the
preview target is tiled, a dot for a cache hit, `⚠n` for n warnings (the last run's and the compiler's), and a red
`⚠` with the code and message for an error.

```json
editor.procgen_badges { "nodes": [2] }
→ { "nodes": [{ "id": 2, "badges": ["visualized", "tier"], "warnings": 0,
                "tier": { "index": 0, "cell_m": 64.0, "shared": false, "feeds": [0], "neutral": false },
                "cache_hit": false, "error": null,
                "pins": { "inputs": [{ "name": "region", "colors": ["#e59e00", "#009e72", …] }],
                          "outputs": [{ "name": "out", "colors": ["#e59e00"] }] } }] }
```

Badge names: `bypassed`, `visualized`, `inspected`, `tier`, `cache_hit`, `warnings`, `error` (`error` is
`{ reason, message }`). A tier-neutral node (a source, or fed only by sources) runs in every tier that consumes it: in
a graph with several tiers its badge names them all (`T0+T1`, ADR-7275), and its `tier` row is
`{ index, cell_m, shared: true, feeds: [0, 1] }` (`index` is the coarsest of them). A node placed at one tier has
`shared: false` and `feeds: [index]`. `neutral` is true for every tier-neutral node; one that feeds a single tier
(`shared: false`) shows `~` before that tier's cell size on the canvas (`~64 m`), so it does not read as a node placed
at that tier. The biome filter view's cursor tooltip gathers the filtered marks once per preview run and looks the
cursor up once per hover change, not every frame. `editor.set_theme { "tokens": { "data_kinds": { "points": "#ff0000" } } }` recolors the pins.

**Override pins.** Any setting can be fed by data: `editor.procgen_pin { "node": 2, "setting": "spacing_m", "show": true }`
shows the input pin `@spacing_m` (the settings panel's **pin** toggle), and the `procgen.graph_edit` op `{ "op": "connect", "from":
[5, "out"], "to": [2, "@spacing_m"] }` wires a `Table` into it. Row 0 of the table's `spacing_m` column (or its only
column) replaces the setting for the run, over a literal value or a `Bind`; a value that does not convert fails the run
with `override_invalid`. A wired pin is always shown; hiding one removes its edge (one undo entry). Result
`{ "node", "shown": [setting] }`.

The palette is `editor.procgen_palette { "search"?: "jitter", "from"?: [2, "out"] }` →
`{ "count", "types": [{ "ty", "category", "doc", "pin" }] }`: a case-insensitive search over id, category and doc; with
`from`, only the types with an input pin accepting that output pin's data kinds, and `pin` names the input a drop would
connect (the panel shows this list when a wire is dropped on empty canvas; choosing a type adds and connects it).

Double-clicking a `graph.call` / `graph.loop` node opens the graph it calls; a breadcrumb above the canvas shows the
call path and returns. Agents use `editor.procgen_open { "enter": 2 }` and `editor.procgen_open { "back": true }`
(the result lists `breadcrumb`; opening by `path` clears it).

### Collapse and expand

```json
editor.procgen_collapse { "nodes": [2, 3], "path": "procgen/rocks_core.pgraph.ron" }
→ { "call_node": 2, "new_graph": "procgen/rocks_core.pgraph.ron", "graph_hash": "…" }
editor.procgen_expand { "node": 2 }
→ { "nodes": [2, 3], "graph_hash": "…" }
```

Collapse writes the selected nodes into a new graph and replaces them with one `graph.call`. Each
outside pin feeding the selection becomes an input (`in_0`, `in_1`, … read through `graph.input`
nodes); each inside pin read outside becomes an output (`out_0`, …, through `graph.output`). Inside
nodes keep their ids and seed salts, and the call node takes the smallest selected id, so the
generator's output hash is the same before and after. Expand is the inverse (the original graph hash
comes back); a graph that other files or other nodes also call needs `"copy": true`. Errors:
`procgen_collapse_invalid` (empty selection, it holds `graph.input` / `graph.output` /
`graph.loop_index`, or it has no single connected boundary: the selected nodes, notes left out, must be one group
joined by edges between them, and no path may leave the selection and come back into it through other nodes, since
the call would then feed itself; the message names the node at fault and nothing is written), `procgen_file_exists`, `procgen_not_call`, `procgen_expand_shared`. Undo restores
the graph; the new file stays on disk.

## Live preview and debugging

```json
editor.procgen_preview { "generator": 4294967301 }
→ { "generator": 4294967301, "follow": false, "refresh_ticks": 9 }
editor.procgen_preview { "follow": true }
→ { "generator": 4294967296, "follow": true, "refresh_ticks": 9 }
```

**Preview target.** While a graph is open and no generator was chosen, the preview follows the default
rule: the selected entity's generator, else the last one used (a selection without a generator keeps the
last one). Choosing a generator, in the preview bar's **Preview on** picker or with `generator` (or
`generator: null`, which unbinds), keeps that choice until the next one; the picker's *Follow selection*
entry, or `follow: true`, returns to the default rule. `follow` in the result says which is in effect.

The panel binds to a generator; edits recompile at once and a valid graph re-runs the target after a
quiet period (`procgen_refresh_ms`, default 150 ms = 9 editor frames at 60 Hz). Several quick edits
make one run; an edit while a run is queued or running cancels it (`preview.cancelled` counts these).
An invalid graph does not run and the last good output stays. Preview runs keep every node's output
(capped at 256 MiB per generator: the oldest nodes in evaluation order are dropped first and listed in
`dropped_debug`). `generator: null` unbinds.

`tier` in `editor.procgen_preview` sets the preview quality tier that `flow.quality` reads (`ultra`, `high`, `medium`,
`low`, `deck`, `mobile_high`, `mobile_low`; `null` returns to `high`; `procgen_bad_tier` otherwise).

**The preview bar** (the panel toolbar, after the document name): *Preview on* (follow selection, none, or a
generator), the quality tier, *Isolate* (with one node selected; *End isolate* while isolating), the
*Around view* and *Tiles* switches (the same as `editor.procgen_view_set`'s `generate_around_view` and
`tiles_overlay`), and with a target bound the **Seed** field (typing a value pins it: one undo entry, and the
generator runs again), Pin, New seed and Same seed as. Every control sets a value rather than toggling, so the
second pass egui may run in one frame cannot apply it twice; the panel also applies one pass's edits per frame.

The seed bar is `editor.procgen_seed { "action": "pin" | "unpin" | "new" | "set" | "same_as", "generator"?, "seed"?,
"from"? }` → `{ "generator", "pinned", "seed" }`: pin the seed in effect, unpin (back to the guid-derived seed), a new
seed, a given `seed`, or the seed of generator `from`. Each is one undo entry and re-runs the generator.
The preview bar's Pin seed, New seed, Same seed as and the tier list call the same functions as that
method and as `editor.procgen_preview`'s `tier` (`apply_seed_action`, `set_tier`). Same seed as lists
the other generators. The tier list's Running entry is `tier: null`. Collapse, expand and isolate on
the node menu call the same functions as `editor.procgen_collapse`, `editor.procgen_expand` and
`editor.procgen_isolate`.

**Visualize** draws a node's output in the viewport as debug lines:

```json
editor.procgen_visualize { "node": 4, "on": true, "color_by": "attr", "attr": "layer", "form": "boxes" }
→ { "visualized": [4], "drawn_points": 16, "total_points": 16, "subsampled": false }
```

`color_by`: `density` (black to white, default), `attr` (a ramp for numbers, categorical colors for
names) or `tags`; `form`: `points` (crosses), `boxes` (oriented bounds) or `axes`. Shapes draw as wire
boxes; masks as a grid of ground crosses (at most 48 × 48, at the terrain height) tinted by value. `debug.show` nodes are
always drawn. The draw lists are rebuilt only when the shown debug data, the styles or the document change. At most 200 000 points are drawn; beyond that an even
subsample of exactly 200 000 is drawn and `subsampled` is true.

**Inspect** fills the data table:

```json
editor.procgen_table { "node": 4, "sort": "density", "descending": false, "filter": "position.y > 0.5",
                       "page": 0, "select_row": 3 }
→ { "node": 4, "pins": 1, "items": 1, "kind": "Points", "tags": [], "columns": ["index", "position.x", …],
    "rows": [[0, -6.0, …], …], "elements": [0, 1, …], "total": 16, "pages": 1, "page": 0, "selected": 3 }
```

Columns are the built-ins (`index`, `position.*`, `rotation.*`, `scale.*`, `density`, `seed`) and every
attribute (vectors split into `.x`, `.y`, …); tables show their columns. Sorting is stable (equal keys
keep element order); filters are `column op value` with `<`, `<=`, `>`, `>=`, `==`, `!=`; pages hold
1000 rows. `select_row` selects the element in that view row (the viewport highlights it);
`select_at: [x, y, z]` selects the nearest point within 1 m and returns its row. `pin` and `item` pick
another output pin or item.

For a **tiled** generator the table shows one cell's data: `cell: [tier, i, j]` picks it (`null` back
to the run's first cell), the reply's `cell` names the cell shown and `cells` lists the run's cells
(`[]` for an untiled generator). A cell outside the run is `procgen_unknown_cell`; a malformed one
`procgen_bad_cell`.

The panel's data table has the same controls: one tab per output pin, an item picker, a filter field (red with the
refusal; a refused filter leaves the last good one in place), ◀ / page / ▶, column headers that sort on a click
(ascending ▲, descending ▼, then unsorted), a first cell that selects its row and — on a tiled target — a
`(tier, i, j)` cell picker. Hovering a row highlights its element in the viewport (a white beam; the
selected element's is gold). They write the same view the agent
reads: `editor.procgen_table {}` without `node` answers the view the panel shows, with `sort`
(`{ "column", "descending" }` or `null`) and `filter`; `"toggle_sort": "density"` is a header click. A filter
change returns to the first page; a refused filter or sort column (`procgen_filter_invalid`,
`procgen_unknown_column`) changes nothing.

**Isolate** ("preview up to here") runs the node and its ancestors for the preview target without
replacing the target's output, and visualizes the node:

```json
editor.procgen_isolate { "node": 2 }   → { "node": 2, "state": "isolated", "nodes": [1, 2] }
editor.procgen_isolate { "node": null } → { "node": null, "state": "off" }
```

## Generator inspector

Selecting an entity with a `ProcGenerator` shows, under the component in the inspector, the run state and counts, a
*stale* flag, the last error, Run / Cancel / Clear / Detach / Bake buttons (the `procgen.run`, `procgen.cancel`,
`procgen.clear`, `procgen.detach` and `procgen.bake` methods), the graph's parameters with the value in effect and
its source (`generator`, `variant:<path>` or `default`) plus an override column, and **Open graph**.

```json
editor.procgen_param { "generator": 4294967301, "name": "spacing", "value": 8.0 }
→ { "name": "spacing", "generator": 4294967301,
    "params": [{ "name": "spacing", "ty": "Float", "default": "4.0", "generator": "8.0", "value": "8.0", "source": "generator" }] }
editor.procgen_param { "generator": 4294967301, "name": "spacing", "reset": true }
editor.procgen_open { "generator": 4294967301 }
```

An override edit is one undo entry and re-runs the generator; `reset` removes it. `editor.procgen_open { "generator" }` is
Open graph: it opens the generator's graph in the panel and binds the preview to the generator.

## Capture

`editor.procgen_capture { "panel": "canvas" | "table" | "report", "out"?: "shots/canvas.png",
"width"?: 1000, "height"?: 640 }` renders the panel offscreen (a Vulkan adapter is needed: lavapipe
works) and returns the PNG base64, or writes it to `out` inside the project or the system temp
directory (`procgen_bad_path` otherwise).

## Placement tools

A placement tool turns gestures into the inputs a graph reads and keeps a generator bound to them. The
generator's graph is the tool's **preset**: a graph whose `flags.tool` names the tool kind
(`procgen_tool_preset_mismatch` otherwise). Built kinds:

- **paint** writes a `PaintedPoints` component (saved, `procgen-painted-points` v2: `points` with world
  `position`, `normal`, a zero-based `layer` 0-7 and a `yaw` in radians about the normal, plus up to eight
  `layer_names`; a v1 component reads with yaw 0 and `regia upgrade-project --write` rewrites it at v2). `in.self` reads
  them as a second item tagged `painted` with the integer attribute `layer`, so a graph selects a layer
  with an ordinary filter (`items.by_tag ["painted"]` → `filter.range { attr: "layer", min: 2, max: 2 }`).
  A brush stamps points on a `spacing_m` grid inside `radius_m` around each stroke center (skipping
  spots closer than half the spacing to a point of the same layer); `erase: true` removes every point
  under the brush; `density` keeps that fraction of the stamps. With `random_yaw` (default on) each stamped
  point gets a yaw drawn from the brush `seed` (default 0) and the millimeter cell it lands in, so the same
  stroke with the same seed stamps the same yaws whatever the stroke order; `in.self` turns the point by its
  yaw about the normal.
- **box** writes a box `Shape` on the target (entity transform at its center, `Box { half_extents }` from
  `min`/`max`; an existing shape keeps its edge band) and binds the generator with `bounds: FromShape`, so
  `in.self` reads the shape. A generator saved by the older tool (its own `Box` bounds, no `Shape`) still
  fits the Box tool and gets a box `Shape` on its next session.
- **path** edits an open `Spline` on the target (spec 64): `points` are projected down with the hit
  filter and appended as `Auto` control points, stored in the target's local space. A target the session
  creates sits at the center of the first batch, so its numbers stay small; the generator's box bounds
  grow to cover every point plus a margin (8 m for a path, 4 m for an area). `closed: true` closes a path
  into a loop. `in.self` hands the spline to the graph tagged `self`.
- **area** is the same with a closed `Spline` (always closed) plus a `Shape` hull of the outline (Y
  flattened, at most 256 points) on the target. `in.self` also reads the closed spline as a polygon, so
  `sample.spline { place: inside }` or `polygon.*` nodes work on it. On a **biome preset** (a graph with a
  `biome.areas` node) the target gets a `BiomeArea { biome, shape: FromSpline }` instead of a generator and a
  hull: `biome` is the call's `biome`, else the preset's `biome` parameter (`procgen_tool_bad_gesture` when
  neither names one). Every generator bound to the preset runs again after each edit, apply, undo and cancel,
  so the biome generators pick the area up. One session is one undo entry, like the other kinds.
- To edit points in an open session, send `move: { "index": 1, "to": [x, y, z] }` (projected down like any
  point), `remove: 2` (a control point index) and `clear: true` (drop every point first). They apply in
  the order clear, move, remove, then `points` append. A bad index answers `procgen_tool_bad_gesture` and
  changes nothing. `apply: true` needs two points for a path and three for an area, else
  `procgen_tool_bad_gesture` and the session stays open.

Six presets ship with the editor (`regia_editor::procgen::presets`); a session that names one the project
does not hold yet gets it written to that path first:

| Preset | Tool | Graph |
|---|---|---|
| `starter/tool_road.pgraph.ron` | path | `in.self` → `items.by_kind [Spline]` → `road.junctions` → `road.ribbon` + `road.markings` over `in.ground` |
| `starter/tool_fence.pgraph.ron` | path | `in.self` → `items.by_kind [Spline]` → `spline.modules` (`post repeat(panel post)`, ends dropped onto `in.ground`) → `spawn.mesh` by the `module` attribute |
| `starter/tool_forest_area.pgraph.ron` | area | `in.self` → `items.by_kind [Spline]` → `sample.spline { place: inside, spacing_m: 5 }` → `spawn.mesh` (two tree meshes) |
| `starter/tool_biome_area.pgraph.ron` | area (biome preset) | the biome starter chain `biome.areas` → `biome.layer` → `biome.resolve` → `biome.exclude` → `biome.spawn`; parameter `biome` (default `starter/biomes/beech_forest.biome.ron`) |
| `starter/tool_rubble_box.pgraph.ron` | box | `in.self` → `sample.region { spacing_m: 1.5 }` → `points.jitter` → `spawn.mesh` (two rock meshes) |
| `starter/tool_painted_props.pgraph.ron` | paint | `in.self` → `items.by_tag [painted]` → `spawn.mesh` (a prop per painted point, turned by its yaw) |

```json
editor.procgen_tool { "tool": "paint", "preset": "starter/tool_painted_props.pgraph.ron",
                      "points": [[0, 5, 0], [4, 5, 0]],
                      "brush": { "radius_m": 1.0, "spacing_m": 1.0, "layer": 2 }, "apply": true }
→ { "entity": 4294967310, "created": true, "applied": true, "painted": 10, "added": 10, "removed": 0,
    "spline_points": 0, "state": "queued" }
editor.procgen_tool { "tool": "path", "preset": "starter/tool_road.pgraph.ron",
                      "points": [[-10, 5, 0], [0, 5, 4], [10, 5, 0]], "apply": true }
→ { "entity": 4294967311, "created": true, "applied": true, "spline_points": 3, "state": "queued", ... }
editor.procgen_tool { "tool": "area", "preset": "starter/tool_forest_area.pgraph.ron",
                      "points": [[-8, 5, -8], [8, 5, -8], [8, 5, 8], [-8, 5, 8]], "apply": true }
editor.procgen_tool { "tool": "box", "preset": "starter/tool_rubble_box.pgraph.ron",
                      "box": { "min": [-2, 0, 1], "max": [4, 3, 5] }, "apply": false }
editor.procgen_tool_cancel {} → { "cancelled": true }
```

The target is `target`, else the selected entity when it already fits (a `PaintedPoints` for paint, a
box `Shape` for box, a `Spline` for path and area); otherwise a new entity named after the preset (`<preset>_01`,
counting up) is spawned with the generator. With the editor preference *edit selected only* an unfit
selection answers `procgen_tool_target_unfit` with the reason. Stroke points are projected straight
down onto the surface under the tool's **hit filter**: `terrain` and `meshes` (both on), `skip_generated`
(on: entities carrying `Generated` are ignored, so a tool never builds on its own output),
`only_entity`, and `physics_mask` (the editor's pick path is mesh and terrain based, so the mask filters
meshes by render layer). A stroke point that hits nothing answers `procgen_tool_no_hit`. Each tool's
filter (all but `only_entity`) is saved in the editor preferences file under `procgen.hit.<tool>.terrain`,
`.meshes`, `.skip_generated` and `.physics_mask` (`<tool>` is `path`, `area`, `paint` or `box`), so it
survives a restart; the panel's tool row edits it and agents use `editor.prefs_set { "values": {
"procgen.hit.paint.skip_generated": false } }` ([Editor preferences](editor-preferences.md)).

The generator re-runs on every call (and, while a session is open, at most `tool_refresh_hz` times a
second in the panel, default 10). `apply: true` commits the whole session as one undo entry (undo
despawns a created entity, redo brings it back); `apply: false` keeps the session open for more calls
of the same tool and preset (`procgen_tool_busy` for another). `editor.procgen_tool_cancel` removes
what the session created and restores what it edited, leaving the level byte-identical; it answers
`{ "cancelled": false }` when no session is open.

### The viewport tool mode "Procedural"

The panel's tool row picks a kind (`path`, `area`, `paint`, `box`), a preset whose `flags.tool` fits it (the
shipped presets and the project's starters), the Area biome, the Paint brush (radius, spacing, density, layer,
random yaw, seed) and shows the kind's hit filter; **Arm** arms the viewport mode and **Cancel tool** leaves it
(cancelling an open session). Agents drive the same mode with `editor.procgen_tool { mode, gesture }`:

```json
editor.procgen_tool { "mode": { "tool": "box", "preset": "starter/tool_rubble_box.pgraph.ron" } }
→ { "armed": { "tool": "box", "preset": "…", "biome": null, "brush": { … }, "error": null },
    "session": null, "outcome": null, "cancelled": false, "pending_strokes": 0 }
editor.procgen_tool { "gesture": { "kind": "press", "at": [-2, 5, -1] } }
editor.procgen_tool { "gesture": { "kind": "release", "at": [4, 5, 3] } }
→ { …, "session": 4294967312, "outcome": { "entity": 4294967312, "created": true, "applied": false, … } }
editor.procgen_tool { "gesture": { "kind": "release", "origin": [1, 2.5, -30], "direction": [0, 0, 1] } }
editor.procgen_tool { "gesture": { "kind": "enter" } }
editor.procgen_tool { "mode": "off" }
```

`mode` is `{ tool, preset, biome?, brush? }` (arming the same tool and preset again only updates the biome and
brush; another one cancels the open session first) or `"off"`. `gesture.kind` is `press`, `move`, `release`
(a pointer ray: `at: [x, y, z]` is a ray straight down onto that point, or `origin` and `direction`),
`backspace`, `close`, `enter`, `escape`, or `brush` (`radius_m`, `layer`, `erase`: the `[` / `]`, digit and
Shift keys). The result reports the armed mode (with its last error), the open session's entity, what the
gesture did and the stroke points still waiting.

- **Path and Area**: a click on empty ground appends one control point, a drag on a control point moves it, Backspace
drops the last point, `C` toggles a path's closed flag (an area stays closed), Enter applies the session as
one undo entry and Escape cancels it. Each gesture calls the same `run_tool` and `finish` as
`editor.procgen_tool` and `editor.procgen_tool_cancel`, one point at a time, so a mouse sequence and the
same scripted calls write one document. A press
that hits nothing notes `procgen_tool_no_hit` and adds no point. Enter with fewer than two path points or
three area points answers `procgen_tool_bad_gesture` and leaves the session open.
- **Paint**: a press starts a stroke, each move at least half the spacing from the last stroke point adds
  one, the release ends it; Shift erases, `[` and `]` shrink and grow the radius by a quarter, digits 1 to
  8 pick the layer. Strokes run in the background: the gesture only queues stroke points, and each editor
  tick stamps them in order under a budget of 2048 brush cells (ray casts), at least one stroke point per
  tick, so a big brush never stalls a frame (the stamping reads the level's surfaces, so it runs between
  frames on the editor thread). Enter stamps what is still waiting, then applies the session as one undo
  entry; Escape drops the queue and cancels.
- **Box**: the first drag draws the footprint on the ground (press on one corner, release on the other; the
  box starts 1 m tall), the next drag sets the height from where the cursor ray passes the footprint's
  center (at least 0.1 m). Each release writes the box through the same `run_tool`; Enter applies.

## Plant preview

A Plant graph (`flags.domain: Plant`, spec 68) open in the panel previews in an asset viewport instead of
the level: the selected node's skeleton or mesh (the last node with plant data when no single node is
selected), drawn as a side view of its branches, with the seed, node, plant count, node count, height,
crown width and triangles. Beside it the **variants** strip shows six seeds side by side (`seeds` ..
`seeds + 5`, the numbering `regia procgen plant --seed S --variants N` writes); a click on one sets the
plant's seed and the viewport shows it. Each variant is one in-memory run of the graph with that run
seed; nothing is written to the level. The panel builds at most one variant per editor frame.

```json
editor.plant_preview { "asset": "starter/plant_bush.pgraph.ron", "seeds": 10 }
→ { "asset": "starter/plant_bush.pgraph.ron", "seed": 10, "node": null, "seeds": [10, 11, 12, 13, 14, 15],
    "variants": [{ "seed": 10, "node": 10, "plants": 6, "nodes": 412, "height_m": 1.31, "crown_width_m": 1.02,
                   "triangles": 9120, "segments": 406, "error": null, "lit": null }, …],
    "shown": { "seed": 10, … }, "builds": 6, "lit_renders": 0 }
editor.plant_preview { "asset": "starter/plant_bush.pgraph.ron", "seed": 12 }
editor.plant_preview { "asset": "starter/plant_bush.pgraph.ron", "lit": true }
→ { …, "variants": [{ "seed": 10, …, "lit": { "width": 192, "height": 192, "coverage": 0.21,
                                             "mean_rgb": [61.0, 83.5, 40.2] } }, …], "lit_renders": 6 }
```

**Lit thumbnails (ADR-7275).** When the editor has a render device and the shown node makes a mesh, each variant
also gets a lit thumbnail: the mesh drawn by the batch thumbnail renderer on the asset viewer's stage (three-quarter
view, key light, sky), bark and leaves in flat colors (leaf cards from both sides). The panel draws one per editor
frame after the variants are built and shows it in the viewport and the strip; without a device, or for a skeleton
node, the skeleton strokes are drawn as before. `lit: true` draws every missing one now and needs a device
(`editor_headless` on a headless editor without an adapter). `lit` in a variant is `{ width, height, coverage,
mean_rgb }`: the share of pixels that differ from the backdrop of their row, and their mean color (0-255), or `null`
when there is no picture.

`editor.plant_preview` (read; heavy: up to seven graph runs) builds every variant at once. `seed` sets the
plant's seed (a click on the strip); `node` shows one node's plant data. A variant whose run fails carries
`error` (`code: message`) and the strip draws the code. Errors: `procgen_unknown_graph` (the graph cannot be
read), `procgen_graph_invalid` (it does not parse), `procgen_wrong_domain` (not a Plant graph),
`procgen_param_type` (`seeds`, `seed` or `node` is not a u32). See also [Procedural generators](procgen.md#plants)
for `procgen.plant_stats` and `procgen.plant_build`.

## Viewport: generate around view, tiles overlay, volumes

`editor.procgen_view_set` flips the procedural viewport switches (spec 66 § 4.1 and § 7, spec 67);
omitted flags keep their value and no params reads them back:

```json
editor.procgen_view_set { "generate_around_view": true, "tiles_overlay": true, "show_volumes": false,
                              "biome_overlay": true }
→ { "generate_around_view": true, "tiles_overlay": true, "show_volumes": false,
    "biome_overlay": true, "anchors": 1 }
```

- `generate_around_view`: the editor camera becomes an anchor for every `Anchored` generator
  (`ProcGenExtraAnchors`), so the scheduler requests the cells around the camera and releases the
  ones it leaves, through the same path a game camera uses. Off removes the editor's anchor (other
  host-supplied extra anchors are left alone); with no editor camera nothing is added. The anchor
  carries the camera's forward and a 16:9 perspective frustum (the editor near and far, clip depth
  `0 ≤ z ≤ w`). A tier with `view_gate` drops cells outside that frustum, including cells behind
  the camera. `view_gate` still defaults to off. Resident
  cells then release on the next steps like any anchor that left.
- `tiles_overlay`: one rectangle outline per cell of every tiled generator, colored like the
  runtime debug view (queued grey, capturing and running amber, applying blue, resident green,
  releasing brown, failed red; a seam-risk cell is magenta), plus each anchor's build and release
  circles.
- `show_volumes`: draws every `Shape` volume (a box as its 12 edges, a sphere as three circles, a
  capsule and a hull as the bounding box the generator reads; an edge band adds a dimmer outer
  form) and every `Spline` curve. The selected entities always draw; a selected spline adds a cross
  on each control point and, for `Free` and `Aligned` points, tangent handles at a third of the
  tangent with a stem to the point. Dragging a control point or a `Free` / `Aligned` handle moves it on
  the horizontal plane through the handle (a handle sits at `P + T / 3`, so the tangent is three times
  the drag). The spline updates live and one undo entry is logged on release. An `Auto` point has no
  tangent grip (`procgen_handle_miss`). Generated splines and the selection both drag; other splines drag
  while volumes are shown.
- `biome_overlay`: outlines every `BiomeArea` (its `Shape`, closed `Spline` or map rectangle). An area with
  `preview_alone` (`biome.preview`) draws in magenta; the biome capture keeps only those areas while
  any is set in the editor, so the preview regenerates them alone.
- `biome_tint`, `biome_bands`, `biome_footprints`, `biome_filtered`: the other biome overlay views
  (spec 65 § 7), each on its own:
  - tint: the claim raster over the areas (at most 16 384 square cells, each resolved at its center
    with the generator's claim rule), each cell crossed in its owning biome's `color`, alpha by the
    owner's weight. Each cell drapes on the terrain under it (its center and corners take the ground height plus
    5 cm, ADR-7275); a cell with no terrain under it is drawn at the owning area's height (a shape area's center).
    Editing a terrain redrapes the cells.
  - bands: the cells a second claim shares (the blend band) outlined in white.
  - footprints: each placed point's core and spread circles, from the last node of the preview run
    whose points carry `core_m` and `spread_m` (at most 4 096 drawn, a uniform subsample beyond).
  - filtered: the candidates each entry filter removed in the preview run, as dim crosses. In
    preview runs `biome.layer` writes them to its second output pin `filtered` (attributes `entry` and
    `filter`, `<index>:<kind>` like the report counter `…|filter:<index>:<kind>`); bake and runtime runs
    record nothing.
- A flag that is not a boolean answers `procgen_bad_view`.

`editor.procgen_biome_overlay` reads the overlay whether or not a view draws; `at: [x, z]` answers the
filter view's hover, the removed candidate nearest to that point within `radius_m` (default 1):

```json
editor.procgen_biome_overlay { "at": [5.0, 0.0] }
→ { "views": { "tint": true, "bands": true, "footprints": false, "filtered": true },
    "claim_cells": 1904, "band_cells": 512, "draped_cells": 1904, "biomes": { "Forest": 1088, "Meadow": 816 },
    "footprints": 0, "footprints_drawn": 0, "filtered": 0, "filters": {}, "hover": null }
```

`draped_cells` counts the cells that lie on a terrain. `filters` counts the marks per `"<entry>|<filter>"`; `hover` is `{ entry, filter, position }` or
`null`. Footprints and marks need a preview run (`editor.procgen_preview`). A bad `at` or `radius_m`
answers `procgen_bad_view`.

## Error codes

Errors answer `F0002` (invalid params) or `F0003` (missing document, node, generator or data) with
`data.reason`:

| `data.reason` | Meaning |
|---|---|
| `procgen_not_open` | No graph is open in the panel (`editor.procgen_open` first). |
| `procgen_file_exists` | `editor.procgen_new` / `editor.procgen_collapse`: the file already exists. |
| `procgen_bad_path` | Not a `.pgraph.ron` path, a `..` or absolute graph path, or a capture `out` outside the project and temp directory. |
| `procgen_io` | Writing a graph file failed (the message has the OS error). |
| `procgen_unavailable` | The editor world has no `ProcGenPlugin`, so a graph file cannot be written. |
| `procgen_unknown_graph` | The graph, starter or preset cannot be read. |
| `procgen_graph_invalid` | The file does not parse, or an expanded graph needs parameters this graph lacks. |
| `procgen_not_starter` | `starter` names a graph whose `flags.starter` is false. |
| `procgen_read_only` | A variant is open: its parent graph is read-only. |
| `procgen_unknown_node` | No node with that id. |
| `unknown_node_type` | The palette has no such node type. |
| `unknown_setting` | `editor.procgen_pin` names a setting the node type does not have. |
| `setting_out_of_range`, `setting_type`, `unknown_setting` | An invalid setting value; nothing was written. |
| `procgen_param_name` | Not a new valid parameter name (`[a-z][a-z0-9_]{0,63}`). |
| `procgen_unknown_param`, `procgen_not_bound` | No such parameter, or the setting is not bound. |
| `procgen_param_value` | `editor.procgen_param` without a value, or `reset` on a graph (only variant overrides reset). |
| `procgen_paste_invalid` | The pasted text is not a graph fragment. |
| `procgen_collapse_invalid` | Empty selection, it holds graph inputs or outputs, or it has no single connected boundary (two unconnected groups, or a path out of the selection and back in). |
| `procgen_not_call`, `procgen_expand_shared` | Not a `graph.call` (or, for `enter`, not a `graph.call` / `graph.loop`), or the called graph is used elsewhere (pass `copy: true`). |
| `procgen_no_conflict` | `take_theirs` with no pending outside change. |
| `procgen_no_preview` | Isolate needs a preview target. |
| `procgen_not_generator` | The preview target is not a generator. |
| `procgen_bad_visualize` | Unknown `color_by` or `form`, or `attr` missing. |
| `procgen_bad_tier` | `editor.procgen_preview` `tier` is not a quality tier name. |
| `procgen_bad_view` | `editor.procgen_view_set` got a flag that is not a boolean, or `editor.procgen_biome_overlay` an `at` that is not `[x, z]` numbers or a `radius_m` that is not a positive number. |
| `procgen_bad_seed` | `editor.procgen_seed` action unknown, or `set` without `seed`. |
| `procgen_no_inspect`, `procgen_no_debug_data`, `procgen_debug_dropped` | No inspected node (also `editor.procgen_table` without `node` before any node was inspected), no preview data yet, or the node's data was dropped by the 256 MiB cap. |
| `procgen_filter_invalid`, `procgen_unknown_column`, `procgen_row_out_of_range`, `procgen_no_point` | Data-table filter, sort column, row or viewport pick problems. |
| `procgen_bad_cell`, `procgen_unknown_cell` | `editor.procgen_table` `cell` is not `[tier, i, j]` or `null`, or names a cell the tiled run does not have. |
| `procgen_bad_panel` | `editor.procgen_capture` panel is not `canvas`, `table` or `report`. |
| `procgen_tool_preset_mismatch` | The preset's `flags.tool` is not this tool. |
| `procgen_tool_target_unfit` | "Edit selected only" is on and the selection does not fit (the reason is in the message). |
| `procgen_tool_no_hit` | A stroke point hit nothing under the tool's hit filter. |
| `procgen_tool_busy` | A session of another tool or preset is open. |
| `procgen_tool_bad_gesture` | Unknown tool, bad points, box or brush (layer 0-7, positive radius and spacing, `seed` a u32), a `move`/`remove` index out of range, a spline over 4096 points, too few points to apply a path (2) or area (3), a biome preset with no biome, an unknown `mode` or `gesture`, a Box footprint with no size, or more than 4096 stroke points waiting. |
| `procgen_wrong_domain` | `editor.plant_preview` names a graph that is not a Plant graph. |
| `procgen_param_type` | `editor.plant_preview` `seeds`, `seed` or `node` is not a u32; `editor.procgen_param` `ty` unknown or a value of another type; `editor.procgen_promote` picks a parameter of another type or a setting that cannot be a parameter. |
| `procgen_param_range` | `editor.procgen_param`: `min` above `max`, a range on a parameter that is not a number, or a bound that is not finite. |

`F0011` answers a capture without a Vulkan adapter; `F0012` a call the editor's permission profile
denies. `editor.procgen_pin` is an `edit_scene` method. The editing forms of three `read` methods need
`edit_scene` too and answer `F0012` under a read-only profile before anything changes:
`editor.procgen_starters { starter }`, `editor.procgen_reload { take_theirs }` and `editor.procgen_capture { out }`
(checked before any capture is made); their reading forms stay `read`.

## Panel culling

The canvas and the data table skip what is off screen. A node more than 240 graph units outside the
visible canvas keeps its slot (wires still reach its pins) but its text is not laid out; the table lays
out the rows in the scroll viewport plus one on each side. `editor.procgen_panel_culling {}` reads what
the last panel frame did:

```json
{ "enabled": true, "nodes_drawn": 34, "nodes_culled": 966, "rows_drawn": 28, "rows_total": 1000 }
```

`{ "enabled": false }` turns culling off (every node and row is drawn, for a before and after
measurement); `true` turns it back on. The counts are those of the last frame the panel drew. Error:
`procgen_bad_param` (`F0002`): `enabled` is not a boolean.
