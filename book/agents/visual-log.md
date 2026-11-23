# Selected-object debug overlay and visual log

Use `editor.debug_overlay` to inspect the active world's primary selection and
`editor.visual_log` to replay recorded per-entity shapes. Both change transient view
state, need `read` permission, and leave scene documents and undo history untouched.
The Selected Object Debug and Visual Log tabs are available through the editor layout.

## Selected object

`editor.debug_overlay { entity?, on? }` toggles the overlay with boolean `on` and
optionally makes the full numeric `entity` id the primary selection. Empty params
read state. The viewport and Outliner share that selection; no independent target
is kept by the panel. During play, use a play-world id, not an edit-world id.

The result contains `on`, `entity`, `source` (`edit` or `play`), `labels`,
`shape_count`, `snapshots`, `queued` and `reason`. In play a selection is queued to
the next editor frame; read again after that frame. A queued entity that has gone
away reports `reason: "entity_not_found"` without selecting another object.

Data includes perception cones and hearing radius, perceived targets, the active
state-tree path, smart-object claims, remaining navigation waypoints, running
abilities and unsuspended applied effects. The compact panel and world-space
label share the same text. Collection rows are capped at 128 per category. Disabled overlays do
no gameplay queries. Colors use the editor's existing overlay theme tokens.

Keyboard: Tab to **Show selected object debug data**, then Space toggles it.
The panel inherits `appearance.ui_scale`.

## Record in a game

Add the `visual-log` feature to `regia_framework` and install
`regia_framework::visual_log::VisualLogPlugin`. Play-in-editor installs it for you.
A system can take `ResMut<VisualLog>` and use:

<!-- ignore-reason: fragment of a game system: `entity`, `log`, `start`, `end`, `position` and `shape` come from the system, and the macro only expands with the `visual-log` feature -->
```rust,ignore
regia_framework::debug_log!(entity, line, log, start, end);
regia_framework::debug_log!(entity, text, log, position, "Reached destination");
regia_framework::debug_log!(entity, shape, log, shape);
```

`shape` is `VisualShape::Line { from, to }` or `VisualShape::Text { at, text }`.
Coordinates are world space. Non-finite coordinates are refused and counted.
The plugin advances frames in `First`, once per app update; `Update` systems log
after it. A logging system placed in `First` must run after `VisualLogFrame`.
Explicit producers can use `VisualLog::begin_frame(frame)` without the plugin.
Repeating a frame preserves its entries; a backwards frame clears the old session.

The ring defaults to **120 frames × 128 entries per frame**, including empty
frames. `VisualLog::new(frames, entries_per_frame)` clamps limits to 1–4096 frames
and 1–1024 entries per frame. Oldest frames are evicted, excess entries within a
frame are dropped, and text is truncated to 512 UTF-8 bytes at a character boundary.
Full entity ids include the generation; despawning does not erase history.
No recording file or content format is created.

Shipping: omit `visual-log`. The module, plugin, storage and recording code are
absent. `debug_log!` becomes an empty expansion: even its argument expressions
are neither evaluated nor type-checked. It must not carry gameplay side effects.
Gate recorder-specific imports and system parameters with the game's development
feature too, or keep the entire resource access inside the macro argument:

<!-- ignore-reason: fragment of an exclusive system: `entity`, `world`, `start` and `end` come from the caller -->
```rust,ignore
regia_framework::debug_log!(
    entity, line,
    world.resource_mut::<regia_framework::visual_log::VisualLog>(),
    start, end
);
```

That resource lookup is also erased when `visual-log` is absent.

## Scrub the recording

`editor.visual_log { entities?, frame? }` selects an array of full numeric entity
ids (maximum 256) and an unsigned integer frame. Duplicate ids are collapsed.
An empty `entities` array draws nothing. A missing or evicted frame draws nothing;
there is no nearest-frame substitution. Empty params read/refresh the view.

Results contain `entities`, `frame`, `entries`, `available_entities`, `frames`,
`source`, `snapshots`, `retained`, `frame_count`, `entry_count`, `dropped` and `queued`.
Each entry contains `entity`, `frame` and `shape`: a line has `kind: "line"`, `from`
and `to`; text has `kind: "text"`, `at` and `text`. Vectors are three-number arrays.
`frames` includes retained empty frames; `available_entities` includes historical
entities with retained entries. The panel keeps its most recent snapshot after
play stops. A closed panel performs no ring scans unless an agent requests one.

During play a refresh runs on the next editor frame. Read again to observe it;
`queued` means this request also asked for the next refresh. The viewport shows
precisely the requested frame's shapes, including while play is paused. Scrubbing
does not rewind the game. Captures use the same line projection as live drawing;
the existing capture rasterizer does not draw text labels.

Keyboard: Tab to **Recorded frame**, use Left/Right to scrub; Tab to an **Entity**
row and press Space to select or deselect it. All controls inherit the appearance
scale and named widgets use the current theme.

Example protocol sequence after a test system logs on frames 10–20:

```json
{"method":"editor.debug_overlay","params":{"entity":4294967297,"on":true}}
{"method":"editor.visual_log","params":{"entities":[4294967297],"frame":15}}
{"method":"editor.visual_log","params":{}}
```

Assert exactly one `entries` item with frame 15 and the expected line endpoints;
scrub frame 21 and assert an empty array. Check `frame_count` and `entry_count`
against the configured limits. These are dispatcher and unit-test contracts in
`gameplay_panels`, `visual_log_panel`, `debug_overlay_selected` and framework
`visual_log` tests; no golden image is needed.

## Errors

- `invalid_params`: wrong argument type, invalid entity bits, oversized entity
  selection or a negative/non-integer frame. Fix the parameters; no partial edit.
- `entity_not_found`: selected edit-world entity is absent, or a queued play-world
  entity no longer exists. Select an entity in the active world. The former is an
  `entity_not_found` RPC error with `data.reason`; the latter is a result `reason`.
