# Panel timings

Panel timings are not a tab you open. The dock times how long every panel takes to draw and keeps
the results; you read them through an agent method, `editor.panel_timings`. They exist to catch a
panel whose draw time grows with the size of the project. This page also describes how the panels
behave in a large project, which is what the timings measure. Depth: [Editor panels at
scale](../../agents/editor-panel-timings.md).

## What the dock records

Each time a panel draws, the dock measures the draw and stores it under the panel's tab title
(`Outliner`, `Output Log`, `Inspector`, ...). Per panel it keeps the last 240 draw times (milliseconds)
and a count of the draws since the editor started or since the last clear. A panel that was never
drawn is not listed. There is no on-screen view, no setting and no keyboard shortcut for the timings.

## Walk-through: find the slow panel

1. Open a large level, or spawn many entities with `scene.spawn` in a loop.
2. Scroll the Outliner, filter the Output Log and edit a few values, so every panel draws for a while.
3. Ask for the timings: `{"method": "editor.panel_timings", "params": {"panel": "Outliner", "last": 30}}`.
   Omit `panel` for every panel; `last` is how many of the newest times to return per panel (1 to 240,
   default 60).
4. Read each panel's `draws`, `last_ms`, `median_ms` and `max_ms`. A panel whose times grow as the
   project grows is the one to look at. Pass `"clear": true` to forget every recorded time after reading.

## Large projects: what the panels do

The Outliner, the Output Log and the Inspector lay out only what is on screen, so their draw time
stays flat as the project grows.

- **Outliner.** The tree is flattened into the listed rows (the search and **Group by category** applied,
  each row's open or closed state respected), and only the rows in view are drawn. The arrow keys work
  on the whole list, on screen or not: **Up** and **Down** move the selection, **Shift+Up** and
  **Shift+Down** extend it, **Left** closes and **Right** opens the selected row. Shift-click selects the
  range between the primary selection and the clicked row. These keys work while the pointer is over the
  Outliner and no text field has focus. A selection made elsewhere (a viewport pick, `editor.select`)
  opens the entity's parents so its row is listed.
- **Output Log.** The panel keeps its own history of the last 100 000 lines, polled from the engine's
  log ring (which keeps 4 096). It has a level choice (**ERROR** to **TRACE**), a target-prefix field and a
  search field. A new line is tested against the filters once. Rows are one line high; click a row to
  show its whole message under the list, and **Close** hides it. Links in a row stay clickable.
- **Inspector.** A list, array, map or set longer than 200 elements shows one page of 100, with buttons
  for the first, previous, next and last page, and an index box with **Go** to jump to the page holding
  an index. An edit on any page records the element's own path.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Read draw times | none | `editor.panel_timings` (`panel`, `last`, `clear`) |
| Read the Outliner's listed rows, open, close or reveal rows | Up, Down, Left, Right in the Outliner | `editor.outliner_rows` (`open`, `close`, `reveal`, `offset`, `limit`) |
| Read log lines | the **Output Log** tab | `logs.read` |
| Read or edit an element of a long list | the Inspector pager | `scene.get_entity`, `editor.set_property` with a path such as `values[150]` |

## Known limits

- The timings are read through the agent method only; the editor shows no timings view.
- Times are held in memory: they are lost when the editor exits, and a panel is listed only after it has
  drawn.
- `last` outside 1 to 240, or a `panel` that is not a string, is refused with error code `F0002`
  (`data.param` names the parameter).

## See also

[Editor panels at scale](../../agents/editor-panel-timings.md), [Outliner](outliner.md),
[Inspector](inspector.md), [Profiler](profiler.md), [Editor guide](index.md).
