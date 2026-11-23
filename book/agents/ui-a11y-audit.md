# UI accessibility audit

`ui.a11y_audit` finds accessibility defects in the screen the UI shows now. It combines what the other checks
look at separately: `ui.theme_lint` checks a theme's token pairs, `ui.nav_lint` the focus graph and
`ui.a11y_tree` the names. A button with an empty name, white text on a light image panel, a 12 px close box or a
label cut off at 200 % text scale can pass all three. The audit looks at the computed layout, the accessibility
tree and each node's resolved theme look together (ADR-3025).

It reads. It adds no system to the frame, so a game that never calls it pays nothing, and the live UI is left as
it was. The 200 % check runs the real layout in a scratch world.

```json
{"method": "ui.a11y_audit", "params": {}}
{"method": "ui.a11y_audit", "params": {"profile": "touch", "rules": ["a11y_target_size"]}}
```

## Parameters

| Parameter | Meaning |
|---|---|
| `root` | Only this node and its subtree (entity bits). Everything still paints, so backgrounds stay right. Omit it for every root. |
| `profile` | `desktop` (default) or `touch`. See `a11y_target_size`. |
| `rules` | The rule codes to run. Omit it for all of them. |
| `min_target` | Replaces the profile's minimum target, in UI px. |
| `text_scale` | The scale `a11y_text_clipped_at_scale` lays out at, 1 to 3. Default 2. |

## Reply

`{ clean, theme, profile, min_target_px, text_scale, reduced_motion, nodes, truncated, errors, warnings, infos,
findings_total, findings, exemptions, skipped }`

Each finding is `{ rule, severity, node, path, measured, required, criterion, hint, needs_eyes }`:

- **`severity`**: `error`, `warning` or `info`. `info` means the audit could not tell and a person has to look.
  It is never a pass, and `clean` is false while one is listed.
- **`node`**: the entity bits, for `ui.query` and `ui.click`. Valid in this run only.
- **`path`**: `NodeName`s, else `role[child index]` (`hud/panel[2]/button[0]`). It names no entity, so it stays
  the same between runs. Compare paths, not node ids.
- **`measured`** and **`required`**: what was found and what the rule needs (`2.10:1` and `4.50:1`).
- **`criterion`**: the WCAG 2.2 success criterion number, or `engine` for the engine's own contract.
- **`needs_eyes`**: true when a person has to check it.

Findings are sorted by rule, then severity, then path, so two runs over the same screen answer the same bytes.

`exemptions` lists `{ rule, path, exemption }` for each node a rule let pass because of a named exception, so a
reviewer can see what was excused. `skipped` lists `{ rule, reason }` for rules that could not run.

`truncated` is true when more than 32,768 nodes were visible. A reply lists at most 4,096 findings, and
`findings_total` counts them all.

## Rules

| Code | Criterion | What it reports |
|---|---|---|
| `a11y_unnamed_interactive` | 4.1.2 | A focusable or pressable node with no accessible name. |
| `a11y_text_contrast` | 1.4.3 | Text against its effective background: 4.5:1, or 3:1 for large text. |
| `a11y_nontext_contrast` | 1.4.11 | The focus ring and a control's boundary against what is next to them: 3:1. |
| `a11y_target_size` | 2.5.8 | A target under the minimum, 24 UI px per side by default. |
| `a11y_focus_unreachable` | 2.4.3 | A node keyboard or gamepad navigation cannot reach. |
| `a11y_no_focus_indicator` | 2.4.7 | A focusable node that would draw no focus indicator. |
| `a11y_text_clipped_at_scale` | 1.4.4 | Text lost when the text scale goes to 200 %. |
| `a11y_motion_ignores_setting` | `engine` | An animation that keeps running with reduced motion on. |

### `a11y_unnamed_interactive`

A node with `Focusable` or `Interactable` whose name in `ui.a11y_tree` is empty, only spaces, or a missing
localization key. Severity is `error` for a focusable node or a node with a role. It is `warning` for a bare
pointer surface with no role, which may be a gesture catcher. Mark that with `Accessible::decorative()` if it
is one. Drag surfaces that hold other things (windows, scroll views, lists, dialogs) are not controls to name
unless they take focus.

Exemption: `decoration`.

### `a11y_text_contrast`

The background comes from walking down the paint order, the way the draw list paints: through solid and
translucent fills, including a panel that is a sibling behind the text and not an ancestor. A translucent layer
painted over the text (a dialog's scrim) tints glyphs and background alike. The ratio is the one `ui.theme_lint`
uses.

| Background | Result |
|---|---|
| Solid or translucent fills ending in an opaque one | `error` when under the ratio |
| A gradient | The worst stop counts. `error` when every stop fails, `warning` with `needs_eyes` when only some do |
| An image, a UI material, or a slider or progress fill | `info`, `needs_eyes` |
| Nothing opaque (the scene or the clear color shows) | `info`, `needs_eyes` |
| Partly over a fill | `info`, `needs_eyes`: the text spans two backgrounds |

An opaque fill over an image makes the background known again. Text under the ratio that has an outline in a
contrasting color is `info` with `needs_eyes`, because the outline gives the glyphs an edge of their own. Large
text is at least 24 UI px, or 18.66 UI px when bold. The live text scale counts.

Exemptions: `inactive_component` (disabled), `decoration` (`Accessible::decorative()`), `not_visible` (fully
transparent, or under an opaque cover).

### `a11y_nontext_contrast`

- **Focus ring:** the node's `focus_ring` color on what surrounds the node, or on its own fill for an inset ring.
  `measured` starts with `focus ring`.
- **Control boundary:** for sliders, toggles, radio buttons, spin boxes, text fields and combo boxes, and for
  focusable nodes with no text. The best of the border, the fill and a slider's or toggle's knob against what
  surrounds the control. `measured` starts with the kind (`text_input boundary`). The themes' `Panel.field` and
  `Button.field` classes have a 3:1 edge: use them for text fields and combo boxes.

An icon button is `info` with `needs_eyes`: its icon identifies it, and the audit does not read pixels. A surface
it cannot resolve is `info` with `needs_eyes` too.

Exemptions: `inactive_component`, `text_identifies_control` (a button with text needs no boundary).

### `a11y_target_size`

`desktop` measures each side of the node's rect in UI px and needs 24. `touch` needs the theme's `size.touch-min`,
or 44 without one, measured in device-independent px: physical px divided by the window's OS scale factor
(ADR-4932). A fingertip has a physical size, and a reference-resolution scale can shrink a UI well below the screen
density. On a 2142x960 phone at OS scale 2.25, the default 1920x1080 reference gives UI scale 1.0, so a 44 UI px
box is about 20 dp and fails. Without a known factor (no `UiScales`), `touch` measures UI px too. The `measured`
and `required` strings keep the `px` unit. `min_target` replaces either minimum, in the profile's unit. You can make a hit box larger than what
it draws: a `regia_ui::TouchTarget { min_px }` on the node grows its hit box to at least `min_px` per side around its
center (ADR-4903), and this rule measures that box. Hit testing uses the same box; layout and drawing do not change,
so space neighbors far enough apart that their boxes do not overlap.

The criterion's exceptions, each listed in `exemptions` when used:

| Exemption | How it applies |
|---|---|
| `spacing` | Measured, `desktop` only: a circle as wide as the minimum, centered on the target, touches no other target and no other small target's circle. A fingertip needs the size itself, so `touch` does not use it. |
| `equivalent` | Declared with the `TargetExemption` component: another control does the same thing and meets the size. |
| `inline` | Declared: the target sits in a sentence or a block of text. |
| `user_agent_control` | Declared: the platform decides the size. |
| `essential` | Declared: this size is essential to what the control conveys. |
| `inactive_component` | The engine's choice, not one of the criterion's five: a disabled control cannot be activated. |

### `a11y_focus_unreachable`

Reuses the lint behind `ui.nav_lint`, on the active scope's graph, from the focused node or the initial node.
`measured` is one of:

- `unreachable` (`error`): no sequence of moves gets there.
- `dead_end` (`warning`): no move leaves the node.
- `not_focusable` (`error`): a button, check box, slider, text field or combo box that takes clicks but has no
  `Focusable`. No keyboard or gamepad can reach it.

### `a11y_no_focus_indicator`

`measured` is one of:

- `no ring` (`error`): no `Themed` look, or a ring with no width or no color. A `Focusable` with no theme class
  has no ring at all.
- `the clip of an ancestor cuts the whole ring` (`error`): the band lies wholly outside the clip. Use an inset ring
  (`border.width.focus-inset`, as list rows do). A partly clipped ring still shows and passes.
- `FocusRingPolicy::Never: the game draws its own` (`info`, `needs_eyes`).
- `no ring; a motion preset moves on focus` (`info`, `needs_eyes`).

Exemption: `inactive_component`.

### `a11y_text_clipped_at_scale`

Lays each root out in a scratch world at `text_scale` and at 100 %, with the same layout code, window size and
fonts as the live UI, and reports text that:

| `measured` | Severity | Meaning |
|---|---|---|
| `... cuts N px of the text` | `error` | A clipping ancestor cuts it |
| `text needs WxH px in a WxH px box that clips` | `error` | Its own box clips it |
| `cut short with an ellipsis` | `error` | It ends in an ellipsis |
| `text ends N px past the screen` | `error` | It leaves the window |
| `text spills N px out of its container` | `warning` | It leaves a filled or bordered parent but stays visible. Not reported when it does so at 100 % too |
| `fit-to-box holds the text at Nx` | `warning` | `ScaleToFit` loses nothing, but the text did not reach the size asked for |

A problem that shows at 100 % as well ends in `(at 100 % too)`. Text reported as lost gets no second line for
spilling. A scroll view or list above the text makes it reachable when its content overflows it and the view fits
the window. A view that grew with its content has nothing to scroll to, so it excuses nothing. A list below the fold
of a page that scrolls stays reachable through the page. Overlap between neighboring nodes is not checked.

Exemption: `captions` (they have their own size option, ADR-3025).

### `a11y_motion_ignores_setting`

A `UiTween` with `follows_reduced_motion` set says its plan already obeys reduced motion, so the tween system
plays it instead of jumping to the end. The rule reports a plan that would still animate anything but opacity, or
fade longer than `motion.fast`. The result does not depend on the setting being on now; `reduced_motion` in the
reply says whether it is. Plain tweens jump to their end value and are never reported.

## Error codes

| Code | Meaning |
|---|---|
| `bad_entity` | `root` is not an entity id. |
| `unknown_entity` | `root` names no entity. |
| `bad_profile` | `profile` is not `desktop` or `touch`. |
| `unknown_rule` | `rules` names a code that is not in the table above. |
| `ui_not_installed` | The app has no runtime UI. |

An unknown parameter or a wrong type answers `F0002`.

## Limits

- The audit does not read pixels: what an image shows always needs eyes.
- A focused state that differs only by its theme `focused` layer is not seen as an indicator. The audit looks for a
  ring.
- A pass is not a screen-reader test. Run the screen on real assistive technology too (see
  [the accessibility guide](../user-guide/accessibility.md)).
- World-space and camera-target roots are audited at their target size, in their own paint stack.
