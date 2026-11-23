# UI tweens and the visual transform (agents)

Guide topic: `agent.guide {"topic": "ui_animation"}`. ADR-273 lane 3, ADR-343.

## Tweens

A tween animates one property of a node from a start value to an end value over a duration, after an optional delay,
through an easing curve. Time comes from the game clock (`Time`): `play.step`, a paused game and a fixed step
reproduce a tween exactly, and nothing reads a wall clock. With reduced motion on (`ActiveTheme::reduced_motion` or the
player's `reduced_motion` setting, see [UI accessibility](ui-accessibility.md)) a tween jumps to its end value on the
next frame. Motion presets ([UI motion presets](ui-motion.md)) follow the spec's rule instead: transforms jump, fades
are capped at `motion.fast`.

| Property | Kind | Writes |
|---|---|---|
| `opacity` | number `0..1` | `Opacity` |
| `background` | colour | A solid `Background` |
| `border_color` | colour | `Border` colour |
| `text_color` | colour | The shaped glyphs while it runs, every span at the end |
| `inset_left`, `inset_top`, `inset_right`, `inset_bottom` | number, UI px | `Style::inset` |
| `width`, `height` | number, UI px | `Style::size` |
| `translate_x`, `translate_y` | number, UI px | `UiTransform::translate` |
| `scale` | number | `UiTransform::scale` |

A start value that is left out is the property's value when the tween starts (for a `width` or `height` that is `auto`,
the laid-out size). Components the node does not have yet (`Opacity`, `Background`, `Border`, `UiTransform`) are added
with neutral values. Several tweens run as a **parallel** group (the default) or a **sequence**; in a sequence a tween
with no start value begins where the previous step on the same property ended. When two tweens in a group write the
same property, the later one wins.

Easing curves: `linear`, and `quad`, `cubic`, `expo`, `back`, `bounce`, `elastic` each as `_in`, `_out` and `_in_out`
(for example `back_out`). A cubic bezier is `[x1, y1, x2, y2]` with the CSS meaning. `back` and `elastic` overshoot
the end value on the way.

Sampling is a pure function (`TweenPlan::sample`): the same time gives the same values. When a tween ends it sends a
`UiTweenFinished { entity }` message and removes itself; replacing it with a new `UiTween` or stopping it sends
nothing. A node with no `UiTween` costs nothing per frame.

Game code starts one by inserting the component:

```text
commands.entity(button).insert(UiTween::new(
    Tween::to(TweenProperty::Scale, TweenValue::Number(1.1), 0.15).ease(Ease::BackOut),
));
commands.entity(panel).insert(UiTween::show(Reveal::Slide([0.0, 24.0]), 0.3, Ease::CubicOut));
```

`UiTween::show` takes a node out of `Display::None` and fades (and slides) it in; `UiTween::hide` fades it out and sets
`Display::None` at the end.

A themed node's colours are written by the theme system when its state changes (hover, press); a colour tween on the
same channel and a theme change fight, and the later write wins. Use tweens for colours the theme does not drive, or
tween a different channel (opacity, scale).

## Visual transform

`UiTransform { translate, scale, pivot_ratio, hit_test }` moves and scales a node and its subtree without layout
knowing: siblings do not shift and the layout result is kept in `UiLayoutBox`. After layout each frame, the computed
rects of the node and every descendant are mapped on the CPU (scale about the pivot, then translate), so the draw list
is built from the transformed rects with no change to the overlay shader. `translate` is in UI px; `pivot_ratio` is a
fraction of the node's own rect (`[0.5, 0.5]` is the centre, the default). A transform on a parent also moves its
children, and a child's own transform applies first.

Limits of this first cut: no rotation (it needs the overlay shader); the scale applies to the boxes, so border width,
corner radius, shadow blur and glyph size keep their size; wrapped text inside a scaled node is shaped again at the new
width.

Hit testing uses the transformed rect by default, so a button is hit where it is drawn. A button that grows or moves
away on hover can lose the hover it just got; set `hit_test: "layout"` on it to keep pointer hit testing on the layout
rect. Focus navigation, `ui.query` bounds and `ui.click` use the transformed rect.

## Methods

`ui.tween { action?, entity?, ... }`. `action` is `query` (default), `start`, `show`, `hide`, `stop` or
`set_transform`. All but `query` need `entity` (entity bits from `ui.query`). Mutating; permission `edit_scene`.

- `start`: one tween `{ property, to, from?, duration?, delay?, ease? }` (duration defaults to 0.25 s), or
  `{ tweens: [ ... ], mode?: "parallel" | "sequence" }` with at most 32. Colours are `"#RRGGBB"`, `"#RRGGBBAA"` or
  `[r, g, b, a]` (linear floats). Starting replaces a running tween on the node.
- `show`, `hide`: `{ kind?: "fade" | "slide", offset?: [x, y], duration?, ease? }`. The default offset is `[0, 24]`.
- `stop`: cancels the node's tween and leaves the values where they are. Answers `stopped: true | false`.
- `set_transform`: any of `translate`, `scale`, `pivot_ratio`, `hit_test` (`"transformed"` or `"layout"`); fields left
  out keep their value. `reset: true` removes the transform.
- `query`: with `entity` the node's state, without it every running tween: `{ count, tweens }`.

Every answer except the list form is `{ entity, tweening, tween, transform, opacity }`; `tween` is `{ elapsed,
duration, progress, properties, shows, hides }` while one runs, else `null`.

Wait for a tween with the `ui.tweening` atom in `play.wait_until` and `play.expect`: `{ predicate: "ui.tweening", entity?,
is? }` holds when the node (without `entity`, any node) is tweening equals `is` (default `true`).

```json
{"method": "ui.tween", "params": {"action": "start", "entity": 4294967301, "property": "opacity", "from": 0, "to": 1, "duration": 0.5, "ease": "quad_out"}}
{"method": "ui.tween", "params": {"action": "start", "entity": 4294967301, "mode": "sequence", "tweens": [{"property": "scale", "to": 1.2, "duration": 0.1}, {"property": "scale", "to": 1.0, "duration": 0.2, "ease": "back_out"}]}}
{"method": "ui.tween", "params": {"action": "hide", "entity": 4294967301, "kind": "slide", "offset": [0, 30]}}
{"method": "ui.tween", "params": {"action": "set_transform", "entity": 4294967301, "translate": [40, 0], "hit_test": "layout"}}
{"method": "play.wait_until", "params": {"condition": {"predicate": "ui.tweening", "entity": 4294967301, "is": false}}}
```

## Errors

| Code | Meaning |
|---|---|
| `missing_entity` | The action needs `entity`. |
| `bad_entity` | `entity` is not entity bits. |
| `unknown_entity` | The entity does not exist. |
| `missing_property` | A tween has no `property`. |
| `unknown_property` | The property name is not one of the table above. |
| `bad_value` | `to`, `from`, `offset`, `translate`, `pivot_ratio`, `scale` or `hit_test` has the wrong type or range. |
| `bad_duration` | `duration` or `delay` is not between 0 and 3600 seconds. |
| `bad_ease` | The curve name is unknown or the bezier is not four numbers. |
| `no_tweens` | `tweens` is empty or not an array. |
| `too_many_tweens` | More than 32 tweens in one call. |
| `bad_mode` | `mode` is not `parallel` or `sequence`. |
| `bad_kind` | `kind` is not `fade` or `slide`. |
| `bad_action` | Unknown `action`. |
