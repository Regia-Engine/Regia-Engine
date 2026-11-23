# UI default widgets: radio, spin box, foldable, split, toast, breadcrumbs

ADR-273 gap rows W05, W06, W07 (exclusive part), Y05, Y06 and the W11 breadcrumbs slice, built on
the theme states of [UI widget style](ui-widget-style.md). No new JSON-RPC method: you read them with `ui.query`, drive them with `ui.click`
and `ui.set_value`, and wait on them with `ui.node_state` / `ui.node_exists`
(see [UI](ui.md) for the filters and the `edit_scene` rule).

## What `ui.query` reports

| `type` | `value` | Notes |
|---|---|---|
| `RadioButton` | `true` when checked | `selected` state is set while checked |
| `SpinBox` | the number | the label child shows prefix, number (fixed decimals) and suffix |
| `Foldable` | `true` when open | the header child (`FoldHeader`) has the `selected` state while open |
| `Split` | ratio of the first pane, `0.01..0.99` | clamped to `min_ratio..max_ratio` |
| `Toast` | none | `text` is the message; `ui.node_exists` with `exists: false` waits for it to expire |
| `Breadcrumbs` | the current segment index | one crumb button per segment with a `›` separator between neighbours; the current crumb has the `primary` class |
| `Tabs` | the active tab index | a `.ui.ron` `tabs` container (`TabPages`, ADR-7827): its first child holds the `TabBar`, child page `i` (`TabPage`) shows only while tab `i` is active |

## Driving them

- `ui.click` on a `RadioButton` checks it and unchecks the others with the same `group` under the same parent.
  `ui.set_value { value: true }` does the same; `false` changes nothing (a group always keeps its choice).
- `ui.set_value { value: <number> }` on a `SpinBox` snaps to the step grid (anchored at `min`), rounds to `decimals` and
  clamps. On a `Split` it sets the ratio (clamped).
- `ui.set_value { value: <bool> }` on a `Foldable` opens or closes it. Opening closes the other open `Foldable`s of the
  same `group` and parent (an accordion). Clicking the header toggles it the same way.
- `ui.click` on a `Toast` dismisses it. A toast with a positive duration expires on its own, counted in game time.
- Dragging a `SplitDivider` moves the ratio by `delta / (split size - divider size)`.
- `ui.set_value { value: <index> }` on a `Tabs` container selects that tab (an index past the last tab changes
  nothing); `ui.click` on a tab button does the same.
- `.ui.ron` documents author all three (v11 fold, ADR-7827): `split((axis, ratio, min_ratio, max_ratio))` (the node's
  first two children are the panes; the divider is added between them), `foldable((title, open, group))` (a header with
  the title is added before the children, which are the body) and `tabs((tabs, active))` (a tab bar is added before
  the children, which are the pages). See `docs/formats/ui-ron.md`.
- On a `SpinBox`: dragging sideways moves one step per `px_per_step`; the mouse wheel steps by the sign of the wheel;
  a click on the left or right quarter (at most 24 px) steps down or up.
- `ui.click` on a crumb of a `Breadcrumbs` navigates to that segment. `ui.set_value { value: <index> }`
  does the same; an out-of-range index is refused with `ui_value_out_of_range`, and the current index
  changes nothing.

Every change emits a `UiEvent` with `UiEventKind::ValueChanged` (`Bool` for radio and foldable, `Number` for spin box and split,
`Index` for breadcrumbs). Setting the value a widget already has emits nothing.

## Toasts from code

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_ui::*;
world.resource_mut::<ToastQueue>().push("Saved", 3.0); // seconds; 0 = until clicked
```

A `ToastHost` node shows up to `max_visible` toasts as children, oldest first; the rest wait in the queue (at most 64,
the oldest are dropped). With no host the queue only fills.

## Errors

`ui.set_value` keeps its codes: `ui_value_type_mismatch` (a `RadioButton` or `Foldable` takes a boolean; a `SpinBox` or
`Split` a finite number; a `Breadcrumbs` an index), `ui_value_out_of_range` (a `Breadcrumbs` index past the
last segment), `ui_node_not_settable`, `ui_value_required`.

`ui.set_value` takes one node selector (`node`, `name`, `text`, `text_contains`, `class`, `type` or `state`) plus
required `value`; booleans drive radio buttons and foldables, finite numbers drive spin boxes and splits, and an index
drives breadcrumbs. A malformed or missing `value` returns
`F0002` with its stable `error.data.reason`; a missing selector match returns `F0003`, and an app without the UI hook
returns `F0013` (`ui_not_installed`).

Workflow: query a radio group, set one option to `true`, then query the group to confirm that it is selected and its
sibling was cleared; use `play.wait_until` with `ui.node_state` to wait for the selected state.

## Not here

The tree view and data grid (W10) are `regia_ui::parity_widgets`, on [UI tree view and data grid](ui-tree.md) and
[UI drawn tree view, data grid and color picker](ui-data-grid.md); menus are on [UI popups and menus](ui-menus.md); the tri-state toggle and
range slider (W07/W08 rest) are later rows.

Captured failure on lavapipe:

Request:

<!-- not-a-request -->
```json
{"jsonrpc":"2.0","id":4,"method":"ui.set_value","params":{}}
```

Response:

```json
{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"name the node: `node` (an id) or filters such as `name`, `text`, `class`, `type`, `state`","data":{"code":"F0002","hint":"call ui.query first to find it","reason":"ui_node_required"}}}
```
