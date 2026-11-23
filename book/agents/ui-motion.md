# UI motion presets (agents)

Guide topic: `agent.guide {"topic": "ui_motion"}`. UI default design D14, ADR-522. Built on
[UI tweens and the visual transform](ui-animation.md).

## What a preset is

A node with `regia_ui::UiMotion` animates four events without game code:

| Trigger | Preset | What moves | Timing (spec § 2.6) |
|---|---|---|---|
| Enter | a reveal: `fade`, `slide` (offset in UI px), `zoom` (scale at the hidden end) | opacity 0 → 1, plus the offset → 0 or the scale → 1 | the node's speed token (`motion.slow` by default) with `ease.enter` |
| Exit | a reveal | opacity → 0, plus → the offset or the scale; then `hide` (`Display::None`) or `despawn` | 0.75 × the speed token with `ease.exit` |
| Hover | a pose: `lift`, `grow`, `sink`, `shrink`, `rest` or `{translate, scale}` | the node's `UiTransform` | `motion.fast` with `ease.standard` |
| Press | a pose | the node's `UiTransform` | `motion.fast` with `ease.standard` |

Durations and curves come from the active theme's tokens (`motion.fast`, `motion.base`, `motion.slow`, `motion.page`,
`ease.standard`, `ease.enter`, `ease.exit`), so a child theme retimes every preset. A token the theme lacks keeps the
spec value. Poses change only the visual transform: layout, siblings and the layout rect stay put.

Named sets:

| Set | Enter / exit | Hover | Press | Speed | Exit action |
|---|---|---|---|---|---|
| `button` | - | - | `sink` (1 px down) | - | - |
| `card` | - | `lift` (2 px up) | `sink` | - | - |
| `popup` | zoom 0.96 | - | - | `slow` | hide |
| `dialog` | slide 12 px | - | - | `slow` | hide |
| `toast` | slide 24 px | - | - | `slow` | despawn |
| `page` | fade | - | - | `page` | hide |

Rules the driver (`run_ui_motion`, `Update`) follows:

- The enter plays when the presets are added (`enter_on_add`, on by default in code, off by default through the agent)
  and on request. An exit plays on request. A node in the middle of an enter or exit turns round from where it is.
- Hover follows the pointer (`Hovered`), or focus in gamepad / keyboard navigation (`Focused` while the input mode is
  navigation). Hover is off in touch mode. Press follows `Pressed`; without a press pose the hover pose stays. A
  disabled node rests.
- A tween the game or `ui.tween` started is not replaced by a pose: the pose follows when it ends. Enter and exit
  replace any running tween.
- An exit with no exit preset hides or despawns at once; an enter with no enter preset shows at once.

## Reduced motion

Reduced motion is on when either source is: `ActiveTheme::reduced_motion` (the host's theme selection) or the player's
setting `GameSettings.accessibility.reduced_motion` (`ui.accessibility {"reduced_motion": true}`, see
[UI accessibility](ui-accessibility.md)). Then:

- Presets follow the spec: transforms (slide, zoom, poses) jump, opacity fades are capped at `motion.fast`.
- Plain tweens (`ui.tween`, game code) jump to their end value.
- A theme swap applies at once instead of cross-fading.

`ui.motion` with no parameters reports both sources and the effective value.

## Method

`ui.motion { action?, entity?, ... }`

| Action | Parameters | Does |
|---|---|---|
| `query` (default) | `entity?` | Without `entity`: `{ reduced_motion: { effective, theme, accessibility }, tokens: { fast, base, slow, page, ease_standard, ease_enter, ease_exit }, presets, poses, count, truncated, nodes }` (at most 64 nodes). With `entity`: the node (below). |
| `set` | `entity`, `preset?`, `enter?`, `exit?`, `hover?`, `press?`, `speed?`, `exit_action?`, `enter_on_add?` | Adds or edits the node's presets. Starts from the named `preset` set, else the node's current presets. A field set to `null` removes that preset. |
| `play` | `entity`, `trigger` (`enter` or `exit`) | Queues the enter or exit; it starts on the next update. |
| `clear` | `entity` | Removes the presets. Answers `{ entity, cleared }`. |

Value shapes: a reveal is `"fade"`, `"slide"` (24 px down), `"zoom"` (0.96), `{"slide": [x, y]}` or `{"zoom": s}`. A pose is
a name or `{"translate": [x, y], "scale": s}` (each optional). Ease values in answers are a curve name or
`[x1, y1, x2, y2]`.

A node answer is `{ entity, motion, state: { phase, pose, pending, running }, tweening, plans, reduced_motion }`.
`phase` is `shown`, `entering`, `exiting` or `hidden`; `pose` is `rest`, `hover` or `press`. `plans` lists the tween each
preset would start now (`{ duration, parts: [{ property, from, to, duration, ease }] }`), with reduced motion applied, so
an agent can check timing without running frames.

```json
{"method": "ui.motion", "params": {"action": "set", "entity": 4294967321, "preset": "dialog"}}
{"method": "ui.motion", "params": {"action": "play", "entity": 4294967321, "trigger": "enter"}}
{"method": "play.wait_until", "params": {"condition": {"predicate": "ui.tweening", "entity": 4294967321, "is": false}}}
{"method": "ui.motion", "params": {"entity": 4294967321}}
```

A `despawn` exit removes the node, so wait for it with the `ui.tweening` atom without `entity`, or with `ui.query`.

## Errors

| Code | Meaning |
|---|---|
| `missing_entity` | `entity` is required for this action |
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `bad_action` | `action` is not `query`, `set`, `play` or `clear` |
| `bad_preset` | `preset` names no preset set |
| `bad_reveal` | `enter` or `exit` has the wrong shape |
| `bad_pose` | `hover` or `press` has the wrong shape |
| `bad_speed` | `speed` is not `fast`, `base`, `slow` or `page` |
| `bad_exit_action` | `exit_action` is not `hide` or `despawn` |
| `bad_value` | `enter_on_add` is not a boolean |
| `bad_trigger` | `trigger` is not `enter` or `exit` |
| `no_motion` | `play` on a node without presets |

## In code

```rust,ignore
use regia_ui::{MotionPose, RevealPreset, UiMotion, UiMotionState};

// A popup that zooms in when spawned and out when closed.
commands.spawn((popup_node, UiMotion::preset("popup").unwrap_or_default()));
// Later: close it.
state.request_exit(); // state: Mut<UiMotionState> of the popup
// A custom tile.
let tile = UiMotion::new()
    .with_reveal(RevealPreset::Slide([0.0, 16.0]))
    .with_hover(MotionPose::GROW)
    .with_press(MotionPose::SHRINK);
```

## Limits

- Poses and reveals share the node's `UiTransform`; a game transform on the same node is overwritten while a preset runs.
- Timing follows the active theme, not a `ThemeScope` subtree theme.
- Scale does not scale border width, corner radius, shadow blur or glyph size (see [UI tweens](ui-animation.md)).
