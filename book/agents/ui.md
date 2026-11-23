# Runtime UI (agents)

An agent can see and drive the game UI (`regia_ui`) the way a person does: find a node, click it, move
focus, type, and wait for a state. Guide topic: `agent.guide {"topic": "ui"}`.

Reads come straight from the world. Writes become input: a click is a pointer move, a button press and a
release fed to the same path a mouse uses, so hover, press, `UiEvent::Click` and every widget reaction run
as they would for a player. Nothing happens in the call itself; the effect shows on the next frame, so
step the game (`play.step`) or use `play.wait_until`.

The `ui.*` methods need the app to have the UI and the agent hooked together. `regia::DefaultPlugins` does
that when the `ui` and `agent` features are on. A host that builds its own app inserts
`regia::agent::UiAgentHost::new(regia::ui::agent_bridge::host_call)`; without it every `ui.*` method
answers `ui_not_installed`.

## Finding nodes

Most methods take the same node filters. Every filter you give must match.

- `node`: the node id (the `id` field of a `ui.query` result).
- `name`: the exact `NodeName`.
- `text`: the exact node text; `text_contains`: part of it, ignoring case. A node's text is its own `Text`
  (localized keys are resolved in the active locale), or, for a button and similar widgets, the text of its
  label children. A label child that only repeats its parent's text is not returned by a `text` filter on its
  own (ask for `type: Text` to find it).
- `class`: a theme class the node carries.
- `type`: `Root`, `Button`, `Toggle`, `Slider`, `ProgressBar`, `TextInput`, `Dropdown`, `TabBar`, `Modal`,
  `ListView`, `ScrollView`, `Text`, `Image` or `Node` (case does not matter).
- `state`: one state name or a list that must all hold: `hovered`, `pressed`, `focused`, `disabled`,
  `selected`, `invalid` (case does not matter).

Methods that act on a node (`ui.click`, `ui.focus`, `ui.set_value`) need the filters to match exactly one.

## Methods

- `ui.query { ...filters, limit? }` lists matching nodes (default 100, at most 1000), in node id order. The
  reply is `{ count, truncated, nodes }`. Each node has `id`, `name`, `type`, `classes`, `rect` and `clip_rect`
  (`[x, y, width, height]` in physical pixels from the root's top-left), `center`, `visible` (the rect and clip
  have an area), `text`, `states`, `value`, `interactable`, `focusable`, `focus_visible` (the node draws its focus ring),
  `layout_dir` (`ltr` or `rtl`, see [UI layout edge cases](ui-layout.md)) and `parent`. `value` is a bool for a
  Toggle, a number for a Slider or ProgressBar, the displayed text for a TextInput (masked when it is a
  password field), the selected index for a Dropdown, the active index for a TabBar, and whether a Modal is
  open.
- `ui.state` reports `focus` (node id or null), `input_mode` (`pointer` or `navigation`), `focus_ring`
  (whether it shows, the policy, and its node, outer rect, width and colour; see
  [UI focus ring](ui-focus-ring.md)), `scale`
  (`window_size`, `scale_factor`, and the scale of each root), `locale` (`selected`, `available`), the `pointer`
  (cursor, hovered and pressed node), the open `tooltip` and the `hints` for the last device (see
  [Tooltips and hints](ui-tooltips.md)), the `nodes` count and `pending_frames` (agent input not yet fed to the UI).
- `ui.click { ...filters, button?, count?, modifiers? }` moves the pointer to the node's center and presses and releases
  the button, `count` times (1 to 8, default 1). `button` is `left` (default), `right` or `middle`; only the left
  button activates widgets today. `modifiers` is a list of `shift`, `ctrl`, `alt` and `meta` held through the
  click (real key messages: down with the first press, up one frame after the last release, so each `UiEvent`
  of the click reports them); a bad entry is `ui_bad_param`. A click takes two frames, plus one when
  `modifiers` is given. The reply has `node`, `at` (the pixel), `top_hit`
  (the node a click at that pixel reaches) and `occluded` (true when another node covers the center), and
  `queued_frames`. Clicking does not move focus: use `ui.focus`.
- `ui.focus { ...filters }` gives the node focus as navigation would: the `Focused` marker and
  `FocusChanged` events. It must be focusable and not disabled. Like a Tab press it also switches the input
  mode to `navigation`, so the focus ring shows; a later `ui.click` switches back to `pointer` and hides it.
- `ui.type { node?, name?, label?, class?, type?, text?, keys? }` types into a node. With a target it focuses
  that node first; without one it uses the focused node. `text` is committed as input-method text (it lands
  in a focused TextInput); then each of `keys` is pressed and released: `Enter`, `Backspace`, `Delete`,
  `Tab`, `Left`, `Right`, `Home`, `End`, a single character, or `Shift+Tab`. Here `label` is the target's exact
  text (the `text` field is the text to type).
- `ui.set_value { ...filters, value }` sets a widget's value: a boolean for a Toggle, a number for a Slider
  (clamped and stepped) or ProgressBar, a string for a TextInput, an index for a
  Dropdown or TabBar. A number must fit the widget's `f32` (`ui_value_out_of_range` past ±3.4e38). A TextInput
  string is sanitized like typed text (line breaks become spaces in a single-line field), cut to its `max_chars` on
  grapheme clusters, and a read-only field or a value the field's filter refuses is left unchanged; the field's undo
  history is cleared, as by `TextInput::set_text` from code. A Toggle, Slider, TextInput, Dropdown or TabBar also
  emits its `ValueChanged` event.

- `ui.set_locale { locale }` switches the active locale (one of `ui.state` `locale.available`). Every
  localized text re-resolves and the layout reflows on the next frame. The reply is `locale`, `previous`,
  `changed` (false when it was already active) and `generation` (moves on every switch).
- `ui.resolve { key, args?, locale? }` formats one message without touching a node. `args` is an object of
  strings and numbers; a number (or a string that is a canonical number such as `"3"`) selects plural forms by
  the locale's rules, so `{ $count -> [one] ... *[other] ... }` works in every language. Without `locale` it
  uses the active locale and falls back to the project fallback; with `locale` it uses exactly that locale. The
  reply is `key`, `locale`, `found` and `text` (`⟦key⟧` in dev builds when nothing has the key). Use it to audit
  a translation: `found: false` for a locale means that locale lacks the key.

`ui.click`, `ui.focus`, `ui.type`, `ui.set_value` and `ui.set_locale` change state: they need the `edit_scene` permission and
are refused with `F0012` in a read-only session before anything is queued. `ui.query` and `ui.state` are
always allowed.

## Waiting

`play.wait_until` and `play.expect` take two node atoms next to the usual ones. They combine with `all`,
`any` and `not`.

- `{ "predicate": "ui.node_exists", ...filters, "exists": true }` holds when at least one node matches
  (`exists: false` waits for none).
- `{ "predicate": "ui.node_state", ...filters, "state": "focused", "is": true }` holds when the one matching
  node has the state (`is: false` waits for it to be gone). No match, or more than one, does not hold.

The other `ui.*` atoms are `ui.theme_generation` ([UI themes](ui-theme.md)), `ui.tweening`
([UI animation](ui-animation.md)) and `ui.theme_fading` ([UI theme switch](ui-theme-switch.md)). `play.expect` and
`play.wait_until` are read-only, so any other `ui.` name (such as `ui.click` or `ui.set_value`) is a broken
condition (`ui_bad_predicate`) and never reaches the UI.

```text
ui.focus {"name": "settings"}
play.wait_until {"condition": {"predicate": "ui.node_state", "name": "settings", "state": "focused"}}
```

## Errors

Errors keep the protocol's code in `data.code` and add a stable `data.reason`. A `ui.*` call can answer:

| `reason` | `data.code` | When |
|---|---|---|
| `ui_not_installed` | `F0013` | the app has no `UiAgentHost` (no UI hooked to the agent) |
| `ui_node_required` | `F0002` | a node-acting call named no node |
| `ui_node_not_found` | `F0003` | no node matches the filters |
| `ui_node_ambiguous` | `F0002` | more than one node matches; add `name`, `type` or `class`, or pass `node` |
| `ui_node_disabled` | `F0002` | clicking, focusing or setting the value of a disabled node |
| `ui_node_not_visible` | `F0002` | clicking a node with no visible area (not laid out, hidden or clipped away) |
| `ui_node_blocked_by_layer` | `F0002` | clicking or focusing a node outside the top input layer, or any node while that layer is `game_only` |
| `ui_node_not_focusable` | `F0002` | focusing a node without focus support |
| `ui_node_not_settable` | `F0002` | `ui.set_value` on a node with no value |
| `ui_value_required` | `F0002` | `ui.set_value` without `value` |
| `ui_value_type_mismatch` | `F0002` | the value type does not fit the widget |
| `ui_value_out_of_range` | `F0002` | a Dropdown or TabBar index past the last entry, or a number outside the f32 range |
| `ui_type_empty` | `F0002` | `ui.type` with neither `text` nor `keys` |
| `ui_no_focus` | `F0002` | `ui.type` with no target and nothing focused |
| `ui_locale_required` | `F0002` | `ui.set_locale` without `locale` |
| `ui_locale_unknown` | `F0003` | `ui.set_locale` or `ui.resolve` with a locale that is not available |
| `ui_key_required` | `F0002` | `ui.resolve` without `key` |
| `ui_unknown_state` | `F0002` | a `state` that is not one of the six names |
| `ui_bad_param` | `F0002` | a parameter of the wrong type or range |
| `ui_unknown_method` | `F0003` | a method the UI does not implement |
| `ui_node_off_window` | `F0002` | `ui.click` or `ui.drag_drop` on a node of a camera-texture or world-panel root (use `ui.world_click`) |
| `ui_world_miss` | `F0003` | `ui.world_click`'s ray hits no world panel |
| `ui_not_world_panel` | `F0002` | `ui.world_click` `entity` is not a world-space root |
| `ui_not_a_root` | `F0002` | `ui.set_target` on a node that is not a `UiRoot` |
| `ui_bad_target` | `F0002` | `ui.set_target` without a valid `target` (window, camera or world with `size` 1..=4096 and `ppm` > 0) |
| `ui_not_a_camera` | `F0002` | `ui.set_target` `{camera}` names an entity without a `Camera` |
| `ui_bad_predicate` | (none: a broken condition) | a `play.expect` / `wait_until` atom names a `ui.*` predicate other than `ui.node_exists`, `ui.node_state` or `ui.theme_generation`; the condition reports broken and its detail ends with `(ui_bad_predicate)` |

## Notes

- The UI and the agent meet through `UiAgentHost`, so the UI crate never names an agent type (ADR-291).
  The agent crate does depend on the UI crate, through `regia_framework` and directly for `ui.layers` and the
  typed `ui.*` helpers (ADR-5665 corrects the earlier note here, which cited ADR-202, the render-out HUD record).
- Queued input is fed one frame at a time, oldest first, before the pointer is read. `ui.state`
  `pending_frames` tells you when it has all been delivered.
- A click leaves the pointer parked on the node, so it stays hovered until the next move.
- Accessibility settings (text scale, reduced motion, high contrast) and right-to-left layout have their own page and
  the `ui.accessibility` method: [UI accessibility](ui-accessibility.md). `ui.state` itself does not report them.

## Camera-target and world-space UI

A `UiRoot`'s `target` decides where its tree draws (ADR-2406):

- `UiTarget::Window` (the default): the window overlay, as before.
- `UiTarget::Camera(camera)`: for a `RenderTarget::Texture` camera, the tree is laid out at the camera's
  texture size (`TextureViewSettings::size`) and drawn into that texture after the view's own passes (the
  frame's `overlay` slot): an in-world monitor that shows a menu, which a material or a `UiImage` then shows.
  For a window camera it is laid out at the camera's viewport rect and drawn over that rect (split-screen
  HUDs); the window pointer reaches it at the offset.
- `UiTarget::WorldSpace { size, ppm }`: the tree is laid out at `size` UI px (`UiRoot::world_space` sets one
  UI px per texel) into an offscreen texture of that many pixels, which the frame draws as a quad
  `size / ppm` metres large at the root entity's `GlobalTransform`: centred (by default) on the origin,
  spanning local X right and Y up and facing +Z, hidden where scene geometry is nearer. It is composited after
  the post stack (`after_post`), so its colours are the authored ones: no exposure, tonemap or bloom. A
  `WorldPanel` component on the root sets `billboard` (`none`, `full`: faces the eye, `axis_y`: turns about
  world up), `pivot`, `fade` (`[start, end]` metres: opaque before `start`, gone after `end`), `depth_test`,
  `opacity` and `live` (redraw the texture every frame, for a panel that shows a live image).

The frame passes `ui_world_panels` (`after_post`) and `ui_camera_targets` (`overlay`) are registered only
while a world or camera-texture root exists, so a frame without such UI does exactly what it did before;
`render.graph` lists them while they exist. Both faces of a panel draw (the back shows the texture mirrored).

The window pointer never hits texture or world UI (their layout rects are target pixels). Focus and gamepad
navigation are unchanged.

- `ui.targets {}` lists every root with its `target` (`window`, `camera`, `world`), layout `size`, and for
  world panels `ppm`, `size_m`, `position`, `billboard`, `pivot`, `fade`, `depth_test`, `opacity`, `live`;
  the offscreen lists the renderer holds (`surfaces`: `kind` `camera_texture` or `world`, `id`, `size`,
  `quads`, `draws`, `composites`); and which of the two passes are registered (`passes`).
- `ui.set_target {root, target}` moves a root. `root` is an entity id or a node name (or the usual filters);
  `target` is `"window"`, `{"camera": <entity>}` or `{"world": {"size": [w, h], "ppm": n, "billboard"?,
  "pivot"?, "fade"?, "depth_test"?, "opacity"?, "live"?}}`. A world target writes the root's `WorldPanel` and
  switches its scale mode to one UI px per texel. The reply is the root's `ui.targets` entry; the change shows
  on the next frame.
- `ui.world_click` clicks a world panel. Give a ray (`origin` and `direction`, or `ray: {origin, direction}`),
  a screen point of a camera (`x`/`y` pixels or `u`/`v` fractions with an optional `camera`, as
  `camera.raycast`), or a panel root `entity` with an optional `uv` (default the centre). A ray is tested
  against every panel (both faces; billboards turn toward the ray origin) and the closest hit wins. The agent
  then casts the same ray with `world.pick`'s scene query: a surface nearer than the panel (by more than 1 cm,
  so a panel mounted on a mesh still clicks) makes the reply `occluded: true` and nothing is clicked.
  `occlusion: false` skips that test. Otherwise the panel pixel goes through that root's own hit test and a
  `Click` is delivered to the interactable node there on the next frame. The reply is `{root, uv, px,
  distance, node, clicked, occluded, occluder_distance}`.

`ui.world_click` and `ui.set_target` need the `edit_scene` permission; `ui.targets` is read-only.

```text
ui.set_target {"root": "shop_sign", "target": {"world": {"size": [512, 256], "ppm": 256, "billboard": "axis_y", "fade": [8, 12]}}}
ui.targets {}
ui.world_click {"x": 640, "y": 360}
ui.world_click {"entity": 4294967321, "uv": [0.25, 0.5]}
```

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
ui.query {"type": "Button", "text_contains": "play", "limit": 10}
ui.state {}
ui.click {"name": "play_button"}
ui.set_value {"name": "volume", "value": 0.5}
ui.type {"name": "player_name", "text": "Ada", "keys": ["Enter"]}
```
