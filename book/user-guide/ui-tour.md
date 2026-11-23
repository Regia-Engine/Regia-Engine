# Runtime UI: a tour

[Runtime UI](ui.md) shows how to author a HUD. This page is the map of everything else `regia_ui` does
today: which part owns which job, where its settings live, and which agent-guide page goes deeper. It lists
the shipped UI lanes of ADR-273 (the runtime lanes L1 to L8 and the default-design lanes D1 to D11). Each
agent-guide page named here has the exact method shapes and error codes.

Not every item below is in every build you may have: the table says which lane added it, and the release
notes (`docs/releases/`) name the version. If a method answers `unknown_method`, the build predates that lane.

## The pieces

| Job | Where it lives | Lane | Read |
|---|---|---|---|
| See and drive the UI from a script or test: find nodes, click, focus, type, wait on a state | `regia_ui::agent_bridge`, `ui.*` methods | L1 | [Runtime UI (agents)](../agents/ui.md) |
| Click kinds (double, context, middle), scroll, long press, repeat; stacked input layers for modals and menus | `UiEvent`, `regia_ui::layers` | L4 | [UI input and layers](../agents/ui-input.md) |
| Text scale, reduced motion, high contrast; right-to-left layout | `UiAccessibility`, `Style` mirroring | L2 | [UI accessibility](../agents/ui-accessibility.md) |
| Tweens, easing, show and hide transitions, a layout-neutral visual transform | `UiTween`, `UiTransform` | L3 | [UI tweens and transform](../agents/ui-animation.md) |
| Popups that escape clipping; dropdowns, tooltips, menus, menu bars, context menus | `regia_ui::popup`, `regia_ui::menu` | L5 | [UI popups and menus](../agents/ui-menus.md) |
| A full text field: selection, clipboard, undo, word moves, caret by mouse | `TextInput`, `UiClipboard` | L6 | [UI text fields](../agents/ui-text-input.md) |
| Keyboard shortcuts bound to named actions, with scopes and conflict reports | `regia_ui::shortcuts` | L7 | [UI shortcuts](../agents/ui-shortcuts.md) |
| Lists of thousands of rows, variable heights, tiles, selection, sticky headers; scrollbars | `ListView`, `Scrollbars` | L8 | [UI lists and scrollbars](../agents/ui-list.md) |
| Theme tokens and `.theme.ron` files | `regia_ui::theme` | D1 | [UI themes](../agents/ui-theme.md) |
| Widget states (hover, pressed, focused, disabled), sizes, density | `Themed`, `WidgetSize`, `UiDensity` | D2 | [UI widget style](../agents/ui-widget-style.md) |
| A page of every widget in every state, for screenshots | `ui_gallery` sample | D3 | [UI theme sheet](../agents/ui-theme-sheet.md) |
| The editor and the game share one set of tokens | editor bridge | D5 | [Editor theme](../agents/theme.md) |
| Theme resolution at runtime: variations, classes, scopes, cross-fade on change | `apply_themes`, `ActiveTheme` | D6 | [UI themes](../agents/ui-theme.md) |
| Read and change themes from an agent, hot reload, lint | `ui.theme_*` | D7 | [UI themes](../agents/ui-theme.md) |
| Motion tokens and the theme panel in the editor | `editor.theme_panel_*` | D8, D10 | [Theme panel](../agents/ui-theme-panel.md) |
| The focus ring and when it shows | `FocusRingPolicy` | D9 | [UI focus ring](../agents/ui-focus-ring.md) |
| Radio buttons, spin boxes, foldables, split panes, toasts | `regia_ui::widgets` | D11 | [UI default widgets](../agents/ui-default-widgets.md) |

Lane D4 (the default icon set and a monospace font) has not shipped. The icon atlas mechanism has (D16,
[UI icons](../agents/ui-icons.md)), but the engine ships no icon artwork: you load your own icon sheets.

## How a frame goes

1. Input arrives (mouse, touch, keyboard, gamepad, or an agent call, which becomes the same input).
2. The top input layer decides which subtree can receive it. A modal, a menu and a popup are layers.
3. Widgets react and send `UiEvent`s. Shortcuts fire here as `ShortcutTriggered` messages.
4. Tweens and theme cross-fades advance on the game clock. A paused or fixed-step game reproduces them exactly.
5. Themes resolve: type, variation, state layers, then `disabled` last. See [widget style](../agents/ui-widget-style.md).
6. Layout runs, then the visual transform is applied to the computed rects.
7. The draw list is built and the overlay draws it.

Because time comes from the game clock, a test steps with `play.step` and compares; nothing needs a wall-clock
wait.

## Settings a player can change

`GameSettings.accessibility` holds `text_scale`, `reduced_motion` and `high_contrast`. The theme picks the
palette and the density. The focus ring follows `FocusRingPolicy`. All of them can be set from an agent, so a
test can check the UI at 150 percent text, in a right-to-left locale, with the high-contrast theme, with the
same script.

## A first menu in code

A menu is a node with the widget attached; the entries are data. Fragments in this chapter are not compiled
(`rust,ignore`); the agent pages show the JSON a test sends.

```rust,wrap
# use regia_ui::menu::{MenuEntry, MenuItem, PopupMenu};
let file_menu = PopupMenu {
    entries: vec![
        MenuItem::new("save", "Save").with_shortcut("Ctrl+S").into(),
        MenuEntry::Separator,
        MenuItem::new("quit", "Quit").into(),
    ],
    ..Default::default()
};
// Spawn it on the node that opens it (a button), next to the node's own bundle.
```

The same menu from a `.ui.ron` file uses `widget: popup_menu([...])`; see
[UI popups and menus](../agents/ui-menus.md). To check it from a test:

```json
{"method": "ui.menu", "params": {"name": "file_menu"}}
{"method": "ui.menu_activate", "params": {"name": "file_menu", "path": ["save"]}}
```

## Checking your UI with an agent

A typical check of a settings screen, written as agent calls. The `ui.query` reply carries the slider's `value`,
which should now read 0.4:

```text
ui.set_value {"name": "volume", "value": 0.4}
play.step {"frames": 2}
ui.query {"type": "Slider", "name": "volume"}
```

The [UI agent map](../agents/ui-map.md) lists every method, the permission each needs, and the error codes by
page.

## What is not done

- Rotation in `UiTransform`, text outline and underline, gradients and group opacity need shader work.
- World-space UI is not built. A tree view and a data grid are: see [UI tree](../agents/ui-tree.md).
- IME composition is stored but not drawn; right-to-left caret movement is logical, not visual.
- The icon mechanism ([UI icons](../agents/ui-icons.md)) and the UI sound table ([UI sounds](../agents/ui-sounds.md)) are built, but the engine ships no icon artwork and no default UI sound set.

The full gap list with status is `docs/specs/ui-parity-roadmap.md`.
