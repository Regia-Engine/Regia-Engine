# UI focus graph and gamepad navigation (agents)

Guide topic: `agent.guide {"topic": "ui_navigation"}`. UI lane 9, ADR-516. Code: `regia_ui::focus_graph`.

Keyboard arrows, Tab, the D-pad, the left stick and the shoulder buttons all move focus through one focus graph. The
graph holds the navigable nodes of the active scope (the top input layer, else the open focus scope): `Focusable`
nodes that are not `Disabled` and have a laid-out rect with area. Nodes are kept in reading order (top to bottom,
then left to right, then entity id), so every tie breaks the same way on every run.

## How a move is decided

For up, down, left and right, from the focused node:

1. **Explicit link.** A `NavLinks` target wins when it is navigable now. A link to a disabled, hidden or
   out-of-scope node falls through to geometry. A link to the node itself blocks that direction (the calibration
   screen uses this for sliders that step their own value).
2. **Innermost group.** If the node sits inside one or more `FocusGroup` containers, the innermost group is searched
   first with the geometric score (nearest in the direction, aligned nodes preferred; `focus::nav_score`).
3. **Edge rule.** When the group has no candidate in that direction, its edge rule decides:

| Rule | Effect |
|---|---|
| `escape` | Search the enclosing group, or the whole scope after the outermost group (the default). |
| `stop` | No move. |
| `wrap` | Jump to the node on the far side of the group, in the same row or column. |

4. **Remembered child.** A move that enters a group created with `remember` lands on the child that had focus last,
   not on the nearest one.

Without any group the result is the same as before ADR-516: geometry over the whole scope, no wrap at the edges.

Next and prev (Tab, Shift-Tab, the right and left shoulder) walk the Tab order: nodes with a positive `TabIndex` first,
ascending, then the rest in reading order. A negative `TabIndex` takes the node out of Tab order; arrows still reach
it. The order wraps at its ends. Inside a group with `tab: cycle`, Tab cycles through that group's members only.

| Preset | Edges | Tab |
|---|---|---|
| `FocusGroup::new()` | all `escape` | `pass` |
| `FocusGroup::trap()` | all `stop` | `cycle` |
| `FocusGroup::wrap()` | all `wrap` | `cycle` |
| `FocusGroup::wrap_horizontal()` | left/right `wrap`, up/down `escape` | `pass` |
| `FocusGroup::wrap_vertical()` | up/down `wrap`, left/right `escape` | `pass` |

`.remembering()` adds the memory to any preset; `.with_edge(direction, rule)` changes one edge.

## Focus that goes stale

- With nothing focused, the first press reveals focus on the default node (`DefaultFocus` inside the scope, else the
  first node in Tab order) without moving.
- If the focused node is despawned, or hidden after it had been visible, while the input mode is `navigation`, focus
  moves at once to the nearest navigable node (else the default). The focus ring never sits on nothing. A node that is
  focused in code before its first layout is left alone.
- In `pointer` mode a despawned focus is cleared; a hidden one is kept (the ring does not show in pointer mode) and
  recovers on the next press.
- A press while focus is outside the active scope (a dialog opened over it) goes to the scope's default node. Before
  ADR-516 such a press did nothing.
- A disabled focused node keeps focus; moves start from its rect.

## Methods

- `ui.nav_graph {}`. Returns `{ scope, focused, input_mode, nodes, groups, count, truncated }`. Each node:
  `{ entity, rect: {x, y, w, h}, tab_index, groups, links: {up, down, left, right}, moves, name, declared }`;
  `name` is the node's name (or `null`); `declared` is `{ up, down, left, right, resolved }` with the neighbor names
  its `.ui.ron` `nav` field declared (ADR-7827; `resolved: false` while a name matches no node of its document), or
  `null`. The resolved targets are in `links`, so a declared name that matched shows there as an entity; `moves` holds
  `up`, `down`, `left`, `right`, `next` and `prev`, each `{ target, reason }`, computed exactly as a press would go now
  (remembered children included). Each group: `{ entity, edges: {up, down, left, right}, tab, remember, remembered,
  members }`. At most 512 nodes; `truncated` says when there are more. Read-only.
- `ui.nav_move { direction }`. `direction` is `up`, `down`, `left`, `right`, `next` or `prev`. Applies one press at
  once through the same graph and switches the input mode to `navigation`, so the focus ring shows. Returns
  `{ direction, from, to, moved, reason, focused, input_mode }`. Mutating; permission `edit_scene`.
- `ui.nav_lint { start? }`. Checks the graph's structure (remembered children are ignored). Returns
  `{ start, nodes, clean, issues: [{ code, node, other, direction }] }`. Reachability starts at `start` (entity bits),
  else the focused node, else the default node. Read-only.

```json
{"method": "ui.nav_graph", "params": {}}
{"method": "ui.nav_move", "params": {"direction": "right"}}
{"method": "ui.nav_lint", "params": {}}
```

A typical check: drive a menu with `ui.nav_move`, confirm the focus with
`play.wait_until {"condition": {"predicate": "ui.node_state", "name": "settings", "state": "focused"}}` (see
[UI](ui.md)), then run `ui.nav_lint` to confirm every button is reachable by controller.

### Reasons

| Reason | Meaning |
|---|---|
| `initial` | Nothing was focused: the press revealed the default node. |
| `recovered` | Focus was gone, hidden or outside the scope and moved to the nearest or default node. |
| `explicit` | A `NavLinks` target. |
| `geometry` | The best geometric candidate. |
| `wrap` | A `wrap` edge jumped to the far side of a group. |
| `remembered` | Entering a remembering group restored its last focused child. |
| `tab` | The next or previous node in Tab order. |
| `stop` | A `stop` edge, or a link to the node itself: no move. |
| `edge` | No candidate and nothing to wrap to: no move. |

### Lint codes

| Code | Meaning |
|---|---|
| `unreachable` | No sequence of moves from the start node reaches this node. |
| `dead_end` | No move leaves this node in any direction, and the graph has other nodes. |
| `dead_link` | An explicit link points to a node that is not navigable now (`other` names it). |
| `one_way_link` | The link target's opposite move does not come back to this node. |
| `duplicate_rect` | Two nodes share one rect, so geometry cannot tell them apart. |

## Errors

| Code | Meaning |
|---|---|
| `unknown_direction` | `ui.nav_move` got a direction other than the six names. |
| `no_focusables` | The active scope has no navigable node. |
| `not_navigable` | `ui.nav_lint` `start` is not in the graph. |
| `ui_not_installed` | The app has no runtime UI (`UiPlugin`). |

Not covered yet: `.ui.ron` has no `FocusGroup` or `TabIndex` field (add them in code). The focus ring's look and
when it shows (pointer or navigation mode) belong to the focus ring work (ADR-372), not to this page.
