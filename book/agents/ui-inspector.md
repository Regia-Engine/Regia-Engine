# UI runtime inspector (agents and editor)

ADR-2703, roadmap row E04. Code: `regia_ui::inspect` (read-only helper), `regia_agent::ui_inspect` (the methods),
`regia_editor::ui_inspector` (the editor panel).

The inspector answers "why does this node look like this, and which node is that?" for the UI of a running game, without
changing anything. Three methods read or outline a node; the editor's **UI Inspector** panel is a client of them.

`ui.inspect` does not read a cached copy of the look. It replays the theme resolution for the one node on request (the
same layers in the same order as the live resolver, recording which layer won each property), then reports the result next
to the node's live values. Nothing runs per frame while the inspector is idle.

## Methods

- `ui.inspect { node | name }` returns the inspection of one node. `node` is the `id` of `ui.query` or `ui.pick_at`
  (`entity` is an alias); `name` is an exact `NodeName` that must match one node. Read only (permission `read`).
  - `layout`: `rect`, `clip_rect` (physical px, `{x, y, w, h}`), `z`, `z_offset`, `paint_order`, `scale`, `content_size`.
  - `path`: the ancestors from the root, `[{ id, name, type }]`.
  - `theme`: `{ classed, theme, scope, classes, layers, rules }` where `layers` is the replay order (`theme defaults`,
    then each type or variation of the class chain, then the matching style rules as `rule <selector>`, then custom
    legacy classes) and `rules` lists the matching `.theme.ron` v2 style rules by selector text with their state
    (`Toolbar Button`, `[hot] :hover`) in apply order. `scope` names a `ThemeScope` theme.
  - `properties`: `[{ name, group, value, source, detail }]`. `group` is `layout` (the node's style), `visual` (the 18
    theme properties `background`, `border_color`, `border_width`, `radius`, `text_color`, `focus_ring`,
    `focus_ring_width`, `focus_ring_offset`, `padding_x`, `height`, `min_width`, `gap`, `text_px`, `icon_color`,
    `elevation`, `opacity`, `translate`, `scale`) or `content` (`text`, `node_opacity`). `source` is one of:

    | `source` | Meaning | `detail` |
    |---|---|---|
    | `default` | nothing set it | empty |
    | `theme` | a theme layer won | the layer, for example `Button.primary hover`, `theme defaults`, `token size.control.md` |
    | `inline` | set on the node | `NodeStyle`, `ThemeOverride`, `Background component`, `Text`, `Opacity` |
    | `animation` | a tween or a running theme colour transition writes it | `UiTween` or the transition |
    | `binding` | a `Bind` writes it | the binding's source path |

    A `null` visual value means the theme leaves the property unset. A layout property is `inline` when it differs from
    `Style::default()`.
  - `bindings`: `[{ source, source_kind, target, mode, format, invert, value, error }]` (`source_kind` is `none`, `resource`,
    `component` or `item`, a repeat item field), with the last pushed value and
    the last error.
  - `focus`: `{ focusable, focused, explicit_links: { up, down, left, right } | null, edges, interactable }`. `edges` is
    where each arrow press would go from this node (the focus graph's geometric rule).
  - `accessibility`: `{ in_tree, name, role, description, hint, value, states }`; `in_tree: false` for a node the
    accessibility tree leaves out (hidden or decorative).
  - `live`: the node's resolved `Themed` snapshot (`generation`, `radius`, `background`, `opacity`, `animating`).
- `ui.pick_at { x, y, top_layer_only?, stack? }` returns `{ x, y, node, path, count, stack }`: the topmost node whose rect
  and clip contain the physical-px point, by the pointer's own rule (higher `z` wins, then later paint order). Unlike a
  click it also finds nodes that are not interactable or are disabled. `top_layer_only` skips nodes outside the top input
  layer. `stack: true` lists every node under the point, topmost first, as `[{ id, name, z }]`. `node` is `null` over
  empty space. Read only.
- `ui.highlight { node | name | clear? }` outlines a node with the layout debug overlay (see
  [UI layout debug overlay](ui-debug-overlay.md)) and turns the overlay on; `clear: true` removes the outline. Returns
  `{ highlight, rect, overlay_enabled }`. Mutating; permission `edit_scene`.

```json
{"method": "ui.pick_at", "params": {"x": 120, "y": 48, "stack": true}}
{"method": "ui.inspect", "params": {"node": 4294967310}}
{"method": "ui.highlight", "params": {"node": 4294967310}}
```

## The editor panel

Window ▸ UI Inspector (panel id `ui_inspector`) attaches to the play-in-editor world (**PIE world**) or, with a project
directory typed in, to a running game over its agent socket (the same loopback endpoint `regia agent call` uses).

- **Tree** with a search box: the search matches kind, name, text, class or a node id, keeps the ancestors of each match
  and opens collapsed nodes while it is active. Collapsed state and the selection survive refreshes; the selection
  clears only when its node is gone.
- **Layout map**: every node's rect scaled to the panel. With **Pick** on, hovering the map calls `ui.pick_at` at the
  matching screen position (so z-order and layers are the game's own) and selects the result; a click picks too.
- **Property table**: the selected node's properties in `visual`, `content` and `layout` groups, each with its value,
  a coloured source and the detail. Selecting a node outlines it in the game through `ui.highlight`.
- **Live refresh** at a capped rate: 4 Hz by default, adjustable from 0.5 to 10 Hz. A refresh is one `ui.query` plus one
  `ui.inspect` for the selected node.

## Errors

| Code | Meaning |
|---|---|
| `ui_bad_param` | `x` or `y` is not a finite number, a flag is not a boolean, or `node` is not an id |
| `ui_node_required` | neither `node` nor `name` was given |
| `ui_node_not_found` | no UI node has that id or name (it may have despawned) |
| `ui_node_ambiguous` | `name` matches more than one node |

## Events and transient live editing (ADR-7811)

The **Events** tab shows the selected node's recent pointer, focus, value, submission and command
events. Each record has a simulation `frame`, monotonically increasing `sequence`, `kind`,
`node` id and `value`. The runtime keeps at most 1,024 records across the UI, evicting the oldest;
text payloads are capped at 1,024 characters. Masked text inputs retain the event kind with a
null text value. Reading never drains the history. This event
recorder runs after widget and command systems, even when the inspector is closed. Style
attribution is still computed only on request.

- `ui.inspect_events {node?, name?}` returns `{node, capacity, events}`. Give the numeric `node`
  or its exact unique `name`, as with `ui.inspect`. Permission: `read`.
- `ui.set_live_style {node?, name?, field, value}` applies a transient edit and returns the refreshed
  inspection. `field` is `text` or a top-level layout field such as `style.size`, `style.padding`,
  or `style.flex_grow`. `value` uses the field's JSON shape; `text` is a string of at most 65,536
  bytes. Validation completes before mutation. Permission: `edit_scene`.

The **Properties** tab provides a field picker, JSON value and **Apply live edit** button. All
controls, the Events tab and **Copy to document** are keyboard operable. The live patch changes
only that runtime node, leaving templates and files unchanged. Runtime bindings, animations or
application code may subsequently replace the live value; editing does not disable them.
`ui.inspect` now includes `events`, `live_edits` and `document`, and each property's ordered
`sources` array retains losing style layers as well as the winner.

**Copy to document** is an explicit authoring operation. It is enabled only for a live patch
with an exact authored origin. The editor checks the source node identity and the original
values of the patched fields, applies one undoable designer batch, and saves the `.ui.ron`
atomically. It refuses unsaved designer changes and stale source values. For an attached game,
the editor must have the same project open; the connected project identity is retained at attach
time. The agent copy route always targets the editor's active project. Code-generated nodes,
expanded component nodes and generated repeat children have no exact inline origin and cannot
be copied. They remain live-editable. This uses the existing file format and save path.

`editor.ui_inspector_copy_to_document {document, edits}` is the same operation, permission
`assets`. Pass the `document` and `live_edits` objects from `ui.inspect` as `document` and `edits`.
The origin contains `path` under the project's assets directory, `node` as an authored path,
and `expected` source fields captured when the node spawned. The result is `{path, node,
copied, undo}`. Undo restores the document in the designer; save to persist that undo.

```json
{"method":"ui.inspect_events","params":{"name":"title"}}
{"method":"ui.set_live_style","params":{"name":"title","field":"text","value":"Preview title"}}
```

Additional error codes: `ui_live_field` (unknown field), `ui_live_value` (invalid value),
`ui_copy_origin` (missing or unsuitable source), `ui_copy_value` (invalid patch),
`ui_copy_conflict` (source changed), `ui_copy_dirty` (unsaved designer work or an entered
component), `ui_copy_io` (open/save failed). Existing `ui_bad_param`, `ui_node_required`,
`ui_node_not_found`, `ui_node_ambiguous` and `path_outside_project` validations still apply.

A text input's live `text` edits its current input value. That value cannot be copied to the
frozen document shape: it has no initial-input-value field. Copy rejects such a patch with
`ui_copy_value` without saving anything; choose a model initializer in application code instead.
Plain text nodes and inline layout fields retain the copy behavior above.
