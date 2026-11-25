# Runtime UI

<!-- index: Runtime UI -->
<!-- index: HUD -->
<!-- index: Theme -->
<!-- index: Focus ring -->
<!-- index: Safe area -->
<!-- index: Toast -->
<!-- index: Screen reader -->
<!-- index: Icon atlas -->

`regia_ui` is the runtime (in-game) UI — HUD, menus, toasts — driven by taffy layout and
cosmic-text, behind `UiPlugin` (added by `DefaultPlugins` when the `ui` feature is on).
The runtime is [spec 18](../../docs/specs/18-runtime-ui.md). The default look is
[the UI default design](../../docs/specs/ui-default-design.md). A task procedure is
[UI and audio](../how-to/play/ui-and-audio.md).

## Authoring

UI is authored as `.ui.ron` and spawned as an entity, or built in code from widgets. The frozen
shape, version 11 since text input options and image fit (`docs/formats/ui-ron.example.ui.ron`; older files still
load and `regia upgrade-project --write` renumbers them), is a `version` and a `root` node with `style`
and `children` — a simple HUD:

```ron,ui
(
    version: 3,
    root: (
        name: "hud",
        style: (direction: column, padding: 16, gap: (8, 8)),
        children: [
            (
                name: "score",
                classes: ["surface"],
                style: (padding: 8, size: (200, Auto)),
                text: (spans: ["Score: 0"], style: (font_px: 22)),
                bind: (source: "Score.value", format: "Score: {:03}"),
            ),
            (
                widget: button,
                interactable: true,
                style: (padding: 10),
                children: [(
                    text: (spans: [Loc("menu-start")]),
                )],
            ),
        ],
    ),
)
```

`spawn_ui` uses only the root's `style` and `children`; put `safe_area` (and `background`, …) on a
child. A `format` keeps literal text around its one placeholder, so `"Score: {:03}"` paints
`Score: 007`.

A node carries `text`, an `image` fill and `style` directly. The `widget:` field adds behavior:
`button`, `toggle`, `slider`, `progress`, `text_input`, `dropdown`, `tab_bar`, `modal`, `tooltip`,
`scroll`, `list`, `focusable`, `popup_menu`, `context_menu`, `menu_bar`, and the touch controls
`stick` and `v_button` — see `docs/formats/ui-ron.md`. Bindings
like `bind: (source: "Score.value", format: "…")` update from reflected state, so a HUD tracks the
world without a system per label.

A text input takes optional limits after its placeholder and multi-line flag:
`text_input("PIN", false, (max_chars: 4, secret: '*', filter: integer))` keeps at most four digits and draws stars
(`read_only: true` allows selecting but not editing). An image fills its node by `fit`: `stretch`, `contain`
(letterboxed, the default) or `cover` (fills the node and crops the overflow, centred).

## Game widgets

A lobby list, a scoreboard, a radar and minimap, world markers and target brackets come ready to use, in code or in
`.ui.ron`: see [Game widgets](game-widgets.md).

## Themes and widgets

Colors, spacing, type scale and per-widget looks come from a `.theme.ron` of design tokens. A theme
can extend another, and the built-ins are `regia-dark` (default), `regia-light`,
`regia-high-contrast` and `regia-light-high-contrast`. See [UI themes (agents)](../agents/ui-theme.md) and
[widget styles](../agents/ui-widget-style.md). The [radial menu](radial-menu.md) is a ready-made
wheel widget.

## Graphs and meters

`UiCanvas { rects }` draws filled rectangles over a node, placed in the node's unit square
(`min`/`max` in 0..1). Changing it redraws only that node's subtree and never runs layout again, so
a graph can update every frame cheaply. The [performance overlay](performance-overlay.md)'s
frame-time graph is one `UiCanvas`.

## Toasts

The event router raises **toasts** — short on-screen messages (a sim event fires one, a settings
change can announce one). They surface as `toast` entries in the agent protocol's
[`events.read`](../agents/events.md), which is how tests assert a message appeared.

## Accessibility

`GameSettings`'s `accessibility` section sets subtitles and UI text scale; text renders through
cosmic-text with script fallback (Latin, Arabic, CJK). Localization is fluent-bundle + BCP-47
(`GameSettings.language`).

The focus ring shows only while the player navigates with a keyboard or gamepad; a mouse click or touch
hides it and keeps focus where it was. Set `FocusRingPolicy::Always` to show it in every input mode (or
`Never` if the game draws its own focus visuals). Its colour, width and offset are the theme tokens
`color.focus-ring`, `border.width.focus` and `border.width.focus-offset`; list rows draw it inset. See
[UI focus ring](../agents/ui-focus-ring.md).

## Safe areas

The `SafeArea` resource reports display cutouts and system insets; the UI keeps interactive widgets
inside it on notched phones and handhelds (`docs/specs/18-runtime-ui.md`).

## Controller navigation

Arrows, Tab, the D-pad, the left stick and the shoulder buttons move focus through one focus graph. Put a `Focusable`
on every interactive node and a `DefaultFocus` on every screen; geometry does the rest. To shape the moves, put a
`FocusGroup` on a container: its children are searched first, and each edge escapes (default), stops or wraps.

```rust,wrap
# use regia::prelude::*;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let (card_row, pause_menu, inventory, play_button, version_label): (Entity, Entity, Entity, Entity, Entity) = todo!();
use regia_ui::{FocusGroup, TabIndex};

// A row of cards that wraps left and right, and a pause menu the D-pad cannot leave.
commands.entity(card_row).insert(FocusGroup::wrap_horizontal());
commands.entity(pause_menu).insert(FocusGroup::trap());
// A list that remembers the row you left when you come back to it.
commands.entity(inventory).insert(FocusGroup::new().remembering());
// Tab visits "Play" first; the version label is skipped by Tab but reachable with the arrows.
commands.entity(play_button).insert(TabIndex(1));
commands.entity(version_label).insert(TabIndex(-1));
```

If the focused node is despawned or hidden while the player uses a controller, focus moves to the nearest node, so the
focus ring is never lost. Agents can inspect the graph with `ui.nav_graph` and check it with `ui.nav_lint`
([UI focus graph](../agents/ui-navigation.md)).

## Themes

Classed nodes take their look from the active `.theme.ron` theme (`regia-dark` by default). A node's classes name a
widget type and variations (`button` is `Button`, `primary` is `Button.primary`); each property comes from the node's
`ThemeOverride`, then the variation, the type and the theme defaults. Swap the theme at runtime by writing the
`ActiveTheme` resource; colors cross-fade over the theme's base motion time.

```rust,wrap
# use regia::prelude::*;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
*world.resource_mut::<regia_ui::ActiveTheme>() = regia_ui::ActiveTheme::named("regia-light");
```

A disabled widget draws at 0.45 opacity (`UiThemes::disabled_opacity`; `1.0` turns it off). Writing your own theme and the
resolution rules: [UI themes](../agents/ui-theme.md).

## Input events and layers

Besides `Click`, a node can raise `DoubleClick`, `ContextClick`, `MiddleClick`, `Scroll`, `LongPress` (with `LongPressable`)
and `Repeat` (with `RepeatOnHold`). A node with `InputLayer` is a layer root: only the top layer's subtree gets hover,
clicks and focus, and an open `Modal` is a layer by itself. Details and the `ui.layers` method:
[UI input](../agents/ui-input.md).

## Motion presets

Add `UiMotion` to a node to animate it on enter, exit, hover and press without writing tweens. `UiMotion::preset` has
`button` (pressed sink), `card` (hover lift, pressed sink), `popup` (zoom), `dialog` and `toast` (slide; a toast despawns
when it leaves) and `page` (fade). Close a node with `UiMotionState::request_exit`. Durations and curves come from the
theme's `motion.*` and `ease.*` tokens. The player's reduced-motion setting (`GameSettings.accessibility.reduced_motion`)
makes moves jump and keeps only a short fade; plain tweens and theme swaps stop animating.

```rust,wrap
# use regia::prelude::*;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# let dialog_node: regia_ui::UiNode = todo!();
commands.spawn((dialog_node, regia_ui::UiMotion::preset("dialog").unwrap_or_default()));
```

Details and the `ui.motion` method: [UI motion presets](../agents/ui-motion.md).

## Driving the UI from an agent

`ui.query`, `ui.state`, `ui.click`, `ui.focus`, `ui.type` and `ui.set_value` find and operate nodes through the same input
path a mouse uses; `play.wait_until` has `ui.node_exists` and `ui.node_state`. See [Runtime UI (agents)](../agents/ui.md).

## Text edge cases

Unwrapped text with `TextOverflow::Ellipsis` ends in "…" on its visual end, the left for right-to-left lines. An emoji
followed by U+FE0F uses the color emoji face; Arabic letters in a mixed label use the Naskh face while the Latin
words keep Inter. CR LF, CR and U+2028 all break a line once, and invisible format characters never draw a box.
Changing a span's color shows on the next frame without reshaping. `ShapedText` reports its lines and
`missing_glyphs`; agents read them with `ui.text_shape`: [UI text shaping](../agents/ui-text.md).

## Screen readers

`regia_ui::a11y` builds an accessibility tree (roles, names, values, states) from the widgets; add `Accessible` to
override a node's role or name, hide decoration, or make a status text a live region. A `ScreenReaderBackend` gets
the changes; on Linux an AT-SPI stub records them (`REGIA_SCREEN_READER=1`). Nothing is built while no reader is
active. Details and `ui.a11y_tree`: [UI accessibility tree](../agents/ui-a11y.md).

## Switching themes at runtime

Labels under themed nodes follow the theme's text color through the cross-fade (add `KeepTextColor` to a `Text` to
keep its color). The player's high-contrast setting selects the high-contrast theme and reduced motion makes the swap
instant. To follow a light or dark preference, set `ThemeMode::follow` and report the OS scheme in
`ThemeMode::system_scheme`. See [UI runtime theme switch](../agents/ui-theme-switch.md).

High contrast keeps the player's scheme: the built-ins include `regia-light-high-contrast` next to
`regia-high-contrast`. On Windows, macOS and Android the OS light/dark appearance arrives as `SystemThemeChanged` and
`ThemeMode { follow: Some(SchemePreference::System), .. }` follows it without host code. The swap fade follows the
theme's `ease.standard` curve and blends in OKLab, so a dark-to-light switch brightens evenly; set the
`ThemeTransition` resource to change its length, curve or blend space. Read `ThemeChanged` and `ThemeFadeFinished`
messages to switch non-UI colors (a clear color, a post-process tint) in step with the UI. A settings menu stores the
player's light/dark choice in `GameSettings` `accessibility.color_scheme`.

## Icons

Load an `IconSheet` (RGBA pages and named drawings such as `gear@16`, `gear@24`) into the `IconAtlas` resource and
spawn `Icon::new("gear")`: the icon picks the drawing that matches its size (`IconSize::Small`, `Medium`, `Large`
follow the theme's icon size tokens), tints with the surrounding text color and recolors on a theme switch. The engine
ships no icon artwork yet. See [UI icons](../agents/ui-icons.md).

## Default design, and what is still open

The default design ([spec file](../../docs/specs/ui-default-design.md), ADR-274) is largely
built. The ADR is still `proposed`. Treat the code and this chapter as the status, and treat
§ 10 of that file as the audit.

Built and covered above: `.theme.ron` themes `regia-dark`, `regia-light`,
`regia-high-contrast`, and `regia-light-high-contrast`; token resolution; the default widgets
that have types; the focus ring in navigation mode; icon atlases once you supply a sheet; theme
swap, including a light high-contrast variant and reduced motion.

Still open. None of these block the four shipped themes. Do not describe them as done:

- `font.mono` names JetBrains Mono, and no TTF for it ships under `regia_ui/assets/fonts`. The
  platform monospace draws.
- The engine ships no default icon glyphs. `<icon=name>` selects the font family `icon-<name>`.
- There is no Checkbox widget. Radio, spin box, foldable, split, tabs, toast, progress, and
  scroll draw with button or panel looks. They have no theme type of their own.
- Toast role bars and hover-to-pause are not built. The compact tree indent and the split view's
  5 percent keyboard step are not built.
- `focus_mode` (`all`, `click`, `none`) is not a setting. Default edge rules for lists, menus,
  and tab strips are not installed for you. `FocusGroup` rules work when the game sets them.
- Gamepad-cursor snapping (`soft_cursor`) is not built.
- Elevation tokens resolve onto the themed node. Nothing draws them as a drop shadow.
- There is no JSON Schema for `.theme.ron`. `Regia.toml` has no `[ui] theme` key. Select a theme
  with `ActiveTheme` or `ui.theme_select`.

Hot reload of a theme polls every 30 frames and reports on the status line, not as a toast.
The `wait_until` predicate is `{ "predicate": "ui.theme_generation", "at_least": n }`. Touch is
not its own `UiInputMode`. Modes are `Pointer` and `Navigation`. An active touch turns the hover
layer off.
