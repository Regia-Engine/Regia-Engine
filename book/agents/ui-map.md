# UI agent map

Every `ui.*` and `editor.theme_panel_*` method of the runtime UI in one place: what it does, the permission it
needs, and the page with the full parameters and error codes. Read [Runtime UI](ui.md) first for the node
filters (`node`, `name`, `text`, `class`, `type`, `state`) that most methods share. The same map for people is
[Runtime UI: a tour](../user-guide/ui-tour.md).

Guide topic: `agent.guide {"topic": "ui"}`.

## Reads (always allowed)

| Method | Returns | Page |
|---|---|---|
| `ui.query` | Matching nodes: rect, clip, text, states, value, `focus_visible` | [Runtime UI](ui.md) |
| `ui.state` | Focus, input mode, scale, locale, pointer, node count, `focus_ring` | [Runtime UI](ui.md), [focus ring](ui-focus-ring.md) |
| `ui.layers` (`list`) | The input layer stack | [Input and layers](ui-input.md) |
| `ui.shortcuts` | Registered shortcuts and their conflicts | [Shortcuts](ui-shortcuts.md) |
| `ui.menu` | The entries of a menu bar, popup menu, context menu or dropdown | [Popups and menus](ui-menus.md) |
| `ui.text_state` | Caret, selection, scroll, undo state of a text field | [Text fields](ui-text-input.md) |
| `ui.theme_list`, `ui.theme_get`, `ui.theme_preview` | Themes, the resolved tokens or widget look, an offscreen screenshot of a page in a theme | [Themes](ui-theme.md) |
| `ui.theme_lint` | Contrast, unknown keys and touch-size findings | [Themes](ui-theme.md) |
| `ui.list` (`info`, `items`) | A list's range, rows, selection, headers | [Lists](ui-list.md) |
| `ui.tween` (`query`) | Running tweens | [Tweens](ui-animation.md) |
| `ui.accessibility` (no parameters) | Text scale, reduced motion, high contrast, locale | [Accessibility](ui-accessibility.md) |
| `editor.theme_panel_state`, `editor.theme_panel_lint` | The editor's theme draft | [Theme panel](ui-theme-panel.md) |
| `ui.nav_graph`, `ui.nav_lint` | The focus graph of the active scope; its unreachable nodes, dead ends and dead links | [Navigation](ui-navigation.md) |
| `ui.text_shape` | How a text node (or a given string) shapes: lines, direction, glyph runs | [Text shaping](ui-text.md) |
| `ui.a11y_tree` | The accessibility tree a screen reader sees | [Accessibility tree](ui-a11y.md) |
| `ui.icons` | The icon atlas, its sheets and the icon nodes | [Icons](ui-icons.md) |
| `ui.glyph_atlas` | The glyph atlas: pages, recycles, glyphs not drawn (thrash) | [Glyph atlas](ui-glyph-atlas.md) |
| `ui.resolve` | One localization message formatted in a locale, touching no node | [Runtime UI](ui.md) |
| `ui.targets` | Every root's target (window, camera texture, world panel) and the offscreen surfaces | [Runtime UI](ui.md) |
| `ui.inspect`, `ui.pick_at` | Why a node looks the way it does; the node under a screen point | [Inspector](ui-inspector.md) |
| `ui.a11y_audit` | Accessibility defects in the screen shown now | [Accessibility audit](ui-a11y-audit.md) |
| `ui.fuzzy` | Labels ranked against a query by the palette's fuzzy matcher | [Fuzzy search](ui-fuzzy.md) |
| `ui.touch` | Gestures recognized from a recorded touch sequence | [Touch gestures](ui-touch.md) |
| `ui.localization`, `ui.text_breaks`, `ui.format_preview` | Locale key coverage; word breaks of SA-class scripts; a number or date formatted in a locale | [Localization](ui-localization.md) |
| `ui.backdrop_stats` | What the backdrop blur did in the last draw | [Backdrop blur](ui-backdrop-blur.md) |
| `ui.layer_stats` | What the group layers did: groups, masks, scale modes | [Group layers](ui-group-layers.md) |
| `ui.components` | The reusable `.ui.ron` components | [Components](ui-components.md) |
| `ui.image_assets` | Nodes whose image or mask names an asset path or icon | [Image assets](ui-image-assets.md) |
| `ui.view_models`, `ui.collection_state` | Registered view models; collection item templates | [UI designer](ui-designer.md) |

## Writes (permission `edit_scene`)

| Method | Does | Page |
|---|---|---|
| `ui.click`, `ui.focus`, `ui.type`, `ui.set_value` | Pointer, focus, text and value input | [Runtime UI](ui.md) |
| `ui.key` | Key chords such as `Ctrl+Shift+Left` | [Text fields](ui-text-input.md) |
| `ui.set_backdrop_blur` | Frosted-glass blur under a node | [Backdrop blur](ui-backdrop-blur.md) |
| `ui.text_select` | Sets caret and selection | [Text fields](ui-text-input.md) |
| `ui.open_popup`, `ui.close_popup`, `ui.menu_activate` | Popups and menu items | [Popups and menus](ui-menus.md) |
| `ui.shortcut_trigger`, `ui.shortcut_contexts` | Fire an action; switch contexts | [Shortcuts](ui-shortcuts.md) |
| `ui.list` (`select`, `scroll_to`), `ui.scroll_to` | Selection and scrolling | [Lists](ui-list.md) |
| `ui.tween` (`start`, `show`, `hide`, `stop`, `set_transform`) | Animate or move a node | [Tweens](ui-animation.md) |
| `ui.layers` (`push`, `pop`) | Push or pop an input layer | [Input and layers](ui-input.md) |
| `ui.accessibility` (with parameters) | Changes the accessibility settings | [Accessibility](ui-accessibility.md) |
| `ui.theme_set`, `ui.theme_select`, `ui.theme_reload` | Edit, switch, reload themes | [Themes](ui-theme.md) |
| `editor.theme_panel_open`, `editor.theme_panel_set`, `editor.theme_panel_save` | Edit and save a theme file in the editor | [Theme panel](ui-theme-panel.md) |
| `ui.nav_move` | One navigation press through the focus graph | [Navigation](ui-navigation.md) |
| `ui.scroll` | Read, scroll or fling a scroll view | [Scroll views](ui-scroll.md) |
| `ui.lobby_list`, `ui.scoreboard`, `ui.radar`, `ui.world_markers`, `ui.target_bracket` | Read or steer the game widgets | [Game widgets](ui-game-widgets.md) |
| `ui.drag_drop` | Report, start, move and drop a drag | [Drag and drop](ui-drag-drop.md) |
| `ui.motion` | Report, set and play enter, exit, hover and press presets | [Motion presets](ui-motion.md) |
| `ui.theme_mode` | Report or change the theme in use, the scheme it follows and the fade | [Theme switching](ui-theme-switch.md) |
| `ui.set_locale` | Switch the active locale | [Runtime UI](ui.md) |
| `ui.a11y_announce` | Say a message through the screen-reader hook (permission `play`) | [Accessibility tree](ui-a11y.md) |
| `ui.a11y_act` | Do what a screen reader's request would: press, raise, lower, set text, focus, close, scroll (permission `play`) | [Accessibility tree](ui-a11y.md) |
| `ui.a11y_platform_tree` | The tree the screen-reader OS bridge sent to the platform (AccessKit roles, actions, states) | [Screen-reader OS bridge](ui-a11y-platform.md) |
| `ui.a11y_platform_connect` | Simulate a screen reader connecting or leaving (permission `play`) | [Screen-reader OS bridge](ui-a11y-platform.md) |
| `ui.a11y_platform_action` | Queue an AccessKit action request as the OS adapter would (permission `play`) | [Screen-reader OS bridge](ui-a11y-platform.md) |
| `ui.set_target`, `ui.world_click` | Move a root to a window, camera or world target; click a world panel | [Runtime UI](ui.md) |
| `ui.highlight`, `ui.debug_overlay` | Outline one node; the layout debug overlay | [Inspector](ui-inspector.md), [Debug overlay](ui-debug-overlay.md) |
| `ui.set_anchor_preset` | Anchor a node inside its parent by preset name | [Anchors](ui-anchors.md) |
| `ui.play_anim` | Timeline animations of keyframe tracks | [Timeline](ui-timeline.md) |
| `ui.screens` | List, push, pop or show the screen stack (permission `play`) | [Screens](ui-screens.md) |
| `ui.subtitles` | The subtitle queue (permission `play`) | [Localization](ui-localization.md) |
| `ui.sounds` | Which cue plays for each UI event | [Sounds](ui-sounds.md) |
| `ui.soft_cursor` | The gamepad-driven software cursor | [Soft cursor](ui-soft-cursor.md) |
| `ui.keyboard` | The on-screen keyboard policy | [On-screen keyboard](ui-keyboard.md) |
| `ui.touch_live` | Route platform touches through the gesture recognizer | [Touch gestures](ui-touch.md) |
| `ui.text_style` | Letter spacing, weight, decorations, features, outline and shadow layers, distance-field text | [Text effects](ui-text-style.md) |
| `ui.material` | A procedural material on a node | [Materials](ui-material.md) |
| `ui.tree`, `ui.data_grid`, `ui.search_box`, `ui.progress_dialog`, `ui.color_picker` | Drive the tree view, data grid, search box, progress dialog and color picker | [Tree view](ui-tree.md), [Data grid](ui-data-grid.md), [Color picker](ui-color-picker.md) |
| `ui.set_group`, `ui.set_scale_mode` | Group opacity, clip and mask; how UI pixels map to physical pixels | [Group layers](ui-group-layers.md) |

`ui.scroll`, `ui.drag_drop`, `ui.motion`, `ui.theme_mode` and the game widget methods also report state when called with no
parameters, but they need `edit_scene` even then. In a read-only session a write is refused with `F0012` before anything is queued. `ui.theme_set` with `persist: true`, and
`editor.theme_panel_open` from a file and `editor.theme_panel_save`, also need the `assets` permission.

## Waiting

`play.wait_until` and `play.expect` take these UI atoms; combine them with `all`, `any` and `not`.

| Atom | Holds when | Page |
|---|---|---|
| `ui.node_exists` | at least one node matches (or none, with `exists: false`) | [Runtime UI](ui.md) |
| `ui.node_state` | the one matching node has the state | [Runtime UI](ui.md) |
| `ui.tweening` | a node (or any node) is tweening | [Tweens](ui-animation.md) |
| `ui.theme_generation` | the theme generation reached a value | [Themes](ui-theme.md) |
| `ui.theme_fading` | nodes are (or, with `is: false`, are no longer) cross-fading after a theme swap | [Theme switching](ui-theme-switch.md) |

## Widgets with no method of their own

The radio button, spin box, foldable, split pane and toast (D11) are read with `ui.query`, driven with
`ui.click` and `ui.set_value` and awaited with the atoms above: [Default widgets](ui-default-widgets.md). The
theme sheet page of the gallery sample gives each widget in each state a node name `sheet.<widget>.<size>.<state>`:
[Theme sheet](ui-theme-sheet.md).

## Errors

Error codes are lowercase ascii, digits and `_`, and each page lists its own. Two rules hold for the whole
family:

- A missing or wrongly typed parameter answers `F0002`.
- `ui_not_installed` means the app has no `UiAgentHost`; `regia::DefaultPlugins` inserts one when the `ui`
  and `agent` features are on.

## A worked check

Open a menu, pick an item, and read that the menu closed:

```text
ui.open_popup {"anchor": {"name": "file_menu"}}
play.step {"frames": 2}
ui.menu_activate {"name": "file_menu", "path": ["save"]}
ui.menu {"name": "file_menu"}
```
