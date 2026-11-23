# UI popups and menus (agents)

Guide topic: `agent.guide {"topic": "ui_menus"}`. ADR-273 lane 5, ADR-332.

## The popup layer

Dropdown lists, tooltips and menus open on the popup layer (`regia_ui::popup`). Each open popup is its own UI root
with `sort_order` 10 000 (plus its nesting depth for submenus), so no owner clip or later root covers it: a dropdown
inside a clipped scroll view opens fully visible.

Placement is a pure function of rectangles (`place_popup`). The panel goes below, above, left or right of its anchor,
flips to the other side when its side lacks room and the other side has more, then shifts on both axes to stay inside
the safe area (the window minus `SafeArea` and the device policy's title-safe margin, `popup_bounds`). Below and above
align left edges; left and right align top edges; a context menu anchors at the pointer. A popup is laid out hidden on
its first frame to measure it, then placed and shown on the next, so it never flashes in the wrong place. A popup
root copies the nearest `ThemeScope` above its anchor.

Interactive popups (menus, dropdown lists) are input layers (`ui_only`): only the top popup gets pointer and
navigation input, and its first item (the selected option of a dropdown, the search field of a long menu) takes focus.
They close on a press outside every open popup panel (a press inside a parent menu closes only the submenus above it),
on `Escape` or gamepad B, when their anchor despawns, and when their parent closes. Closing gives focus back to the
node that opened the popup. Tooltips are passive: no input, no focus, no outside-press closing.

## Widgets

| Widget | Opens | Notes |
|---|---|---|
| `Dropdown` | click, Enter or A | List below the dropdown (above near the bottom edge), at least as wide. Type-ahead: letters typed while the dropdown or its list has focus pick the option starting with them; one repeated letter cycles; a pause of 1 s starts over. |
| `Tooltip` | hover for `delay` s, or focus in navigation mode for `focus_delay` s (0.8) | Hidden while the host is pressed. |
| `PopupMenu` | click, Enter or A on the host | `placement` picks the side. |
| `ContextMenu` | secondary click or long press (at the pointer); `ContextMenu` key, Shift+F10 or gamepad Y on a focused node inside the host (below it) | |
| `MenuBar` | click on a title | Hovering another title while a menu is open switches menus; Left/Right in a top-level menu move to the neighbouring menu. |

Menu entries are `MenuEntry::Item(MenuItem)` or `MenuEntry::Separator`. An item has `id` (the path segment; empty uses
the label), `label`, `enabled`, `checkable`, `checked`, `shortcut` (display text; when empty, the row shows the chord
[`ui.shortcuts`](ui-shortcuts.md) binds to the item's id, or its label without one; ADR-5665) and `submenu`. Keyboard and gamepad: Up/Down move, Enter or A activates, Right or A on an item with a submenu opens it,
Left or B closes the submenu. Hovering an item with a submenu opens it. A menu with more than 12 entries (or
`MenuSearch::Always`) starts with a search field that hides items whose label does not contain the text; Enter in the
field activates the first item left.

Activating an item flips a checkable item, sends `ValueChanged(Text(path))` on the host (item ids joined with `/`),
records a `MenuActivation { host, path, checked }` in the `MenuActivations` resource (dropped at the start of the
second `ui_widgets` run after it, so every system sees it for at least one frame) and closes the menu. Looks come from the theme types `Menu`, `MenuBar`, `MenuItem`,
`MenuItem.checked`, `MenuItem.shortcut`, `Separator` and `Tooltip`.

`.ui.ron`: `widget: popup_menu([...])`, `widget: context_menu([...])` and `widget: menu_bar([...])` take entries
`(id: "save", label: "Save", shortcut: "Ctrl+S", checkable: false, checked: false, enabled: true, submenu: [...])` or
`(separator: true)`. A menu bar's entries are its menus (their `submenu` is the menu). The menu widgets are `.ui.ron`
version 2 (ADR-1381); v1 files load unchanged and `regia upgrade-project --write` renumbers them.

## Methods

- `ui.open_popup { anchor, popup?, placement? }`: opens what the anchor hosts. `anchor` is a node id or a filter object
  (`{"name": "file_menu"}`). `popup` picks `dropdown`, `tooltip`, `menu`, `context`, or a menu bar title (label, id or
  index). An agent-opened tooltip stays until `ui.close_popup`. Returns the popup (`popup`, `panel`, `kind`, `anchor`,
  `opener`, `parent`, `interactive`, `placement`, `placed` with `rect`, `flipped`, `shifted` once placed, and `host`
  and `menu_path` for menus), `already_open` and the open `stack`. Opens at once; `placed` is `null` until the next
  frame. Mutating; permission `edit_scene`.
- `ui.close_popup { popup? }`: closes the newest popup, or the one whose popup or panel id is given, and its
  submenus. Returns `{ closed, focus, stack }`. Mutating; permission `edit_scene`.
- `ui.menu { node, menu? }` (or `name`, `text` or `class` in place of `node`, as for the other `ui.*` node
  methods): lists the entries of a `MenuBar`, `PopupMenu`, `ContextMenu` or `Dropdown` node (or of
  an open menu's popup, panel or row). Each item: `index`, `separator`, `id`, `label`, `path` (ids from the top),
  `enabled`, `checkable`, `checked`, `shortcut` (the text the row shows, registry chord included), `submenu`. A menu bar lists its menus as items; a dropdown lists its
  options with the selected one checked. Also `kind` (`menu_bar`, `popup_menu`, `context_menu`, `dropdown`), `open`,
  `popup` and the `stack`. `menu` picks `popup` or `context` when a node hosts both. Read only.
- `ui.menu_activate { node, path, menu? }`: activates an item as a click does. `path` is a list of ids, labels
  (case-insensitive) or indices, or one string joined with `/`; a menu bar path starts with the menu. On a dropdown the
  path is one option and selects it. Returns `{ node, path, id, checked, closed }`. Works whether the menu is open or
  not; the host's open menus close. Mutating; permission `edit_scene`.

```json
{"method": "ui.menu", "params": {"name": "main_menu"}}
{"method": "ui.menu_activate", "params": {"name": "main_menu", "path": ["view", "grid"]}}
{"method": "ui.open_popup", "params": {"anchor": {"name": "main_menu"}, "popup": "File"}}
{"method": "ui.close_popup", "params": {}}
```

## Errors

Errors carry `data.reason`; the node filter errors of `book/agents/ui.md` (`ui_node_required`, `ui_node_not_found`,
`ui_node_ambiguous`) apply too.

| Reason | Meaning |
|---|---|
| `ui_popup_unavailable` | The anchor hosts no popup of the asked kind. |
| `ui_popup_not_found` | `popup` is not an open popup or panel id. |
| `ui_no_popup_open` | `ui.close_popup` with nothing open. |
| `ui_menu_not_found` | The node hosts no menu, or the menu bar has no menu by that name. |
| `ui_menu_path_not_found` | A path segment names no item. |
| `ui_menu_item_disabled` | The item is disabled. |
| `ui_menu_item_has_submenu` | The item opens a submenu; add one of its items to the path. |
| `ui_bad_param` | A parameter has the wrong type or value (`path`, `popup`, `placement`). |
