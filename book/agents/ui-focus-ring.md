# UI focus ring

The focus ring shows a keyboard or gamepad user which `regia_ui` widget has focus. It follows
`docs/specs/ui-default-design.md` §§ 2.2, 3 and 5 (lane D9, ADR-372; gap I05).

## When it shows

Focus and the ring are separate:

- **Focus** moves with navigation (arrows, Tab, the D-pad, the left stick), with `ui.focus`, and in code
  (`FocusState`, the `Focused` marker). A pointer move, click or touch never moves or drops focus.
- **The ring** draws on the focused node only while `FocusRingPolicy` says so. The policy is a resource:

| Policy | Agent name | Ring shows |
|---|---|---|
| `NavigationOnly` (default) | `navigation_only` | in `UiInputMode::Navigation` only |
| `Always` | `always` | whenever a node is focused (an accessibility setting) |
| `Never` | `never` | never (the game draws its own focus visuals) |

The last device used sets the input mode. Any keyboard navigation key or gamepad input switches to
`navigation`. A cursor move, a mouse button or a touch switches to `pointer`. So with the default policy a
click hides the ring, focus stays where it was, and the next arrow press shows the ring again on the same
node.

The theme's `focused` state layer applies whenever the node has focus, ring or not. A widget that wants a
focus look after a click (a text field's accent border) puts it there. The `focus_visible` layer (ADR-7827) applies
over `focused` only while the ring shows (keyboard or gamepad focus under the default policy), so a clicked control can
keep a quieter look than one reached by navigation: `StyleClass::focus_visible` in code-built classes and
`ThemeOverride`; `StyleState::focus_visible` says whether it applies, and `ui.inspect`'s style replay names the winning
layer `focus_visible`. A `.theme.ron` file cannot set it yet (that needs a theme format step).

## How it looks

| Token | Default (dark / light) | High contrast | Meaning |
|---|---|---|---|
| `color.focus-ring` | `#9cd3ff` / `#0b4f9e` | `#ffd400` / `#002a6e` | ring colour |
| `border.width.focus` | 2 | 3 | band width (UI px) |
| `border.width.focus-offset` | 2 | 2 | gap between the widget's border box and the ring |
| `border.width.focus-inset` | -2 | -3 / -2 | the offset list rows use: minus the width, so the band lies just inside the row |

The ring is concentric with the widget: its inner edge has the widget's corner radius plus the offset,
and its outer radius is clamped to half the ring's short side (a pill keeps a pill ring). A widget type
overrides the ring with the `focus_ring` property (`[colour, width, offset]` in any order of colour and
numbers, width first) or with `focus_ring_width` / `focus_ring_offset`. `List.row` (and every variation
that extends it, such as `Panel.list-item`) sets `focus_ring_offset: "@border.width.focus-inset"`, so a
scroll clip cannot cut the ring of a row.

Slider, toggle and progress fills use the theme's `color.accent` (`Themed::accent`), not the ring colour.

## Agent surface

- `ui.state` has `focus_ring`:
  `{ visible, policy, node, rect, width, color }`. `visible` is true when the focused node draws its ring.
  `rect` is the ring's outer edge `[x, y, width, height]` in physical pixels, `width` is the band width in
  physical pixels and `color` is sRGB hex. They are null when no ring shows.
- `ui.query` nodes have `focus_visible`.
- `ui.focus` switches the input mode to `navigation`, like a Tab press, so the ring shows on the next frame.
  `ui.type` with a target focuses it the same way. `ui.click` switches to `pointer` and hides the ring.

Check that a click hides the ring and keeps focus:

```json
{"method": "play.pause", "params": {}}
{"method": "ui.focus", "params": {"name": "settings"}}
{"method": "play.step", "params": {"frames": 2}}
{"method": "ui.state", "params": {}}
{"method": "ui.click", "params": {"name": "play"}}
{"method": "play.step", "params": {"frames": 3}}
{"method": "ui.state", "params": {}}
```

The first `ui.state` has `input_mode: "navigation"` and `focus_ring.visible: true`. The second has
`input_mode: "pointer"`, the same `focus` and `focus_ring.visible: false`.

There is no method that sets the policy. A game sets it in code:
`world.insert_resource(FocusRingPolicy::Always)`.

## Notes

- A mode flip re-resolves only the focused node, from the style cache: no other node does any work.
- Geometry is `regia_ui::focus_ring_geometry`, used by both the extract pass and `ui.state`.
- Ring goldens per widget are not blessed yet; the headless checks in `crates/regia_ui/tests/focus_ring.rs`
  read the ring quads from the draw list.
