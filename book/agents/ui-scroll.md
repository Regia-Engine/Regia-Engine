# UI scroll containers (agents)

Guide topic: `agent.guide {"topic": "ui_scroll"}`. ADR-520 (UI design series d-12). Lists and the interactive
scrollbars themselves are on the [UI lists and scrollbars](ui-list.md) page.

## Scroll containers

A `ScrollView` scrolls its children; `ScrollOffset` holds the offset (physical px, positive moves toward the end of
the content). Adding a `ScrollView` also adds `ScrollOffset`, `ScrollVelocity` and `ScrollMotion` (gesture state).
`regia_ui::scroll_container(style, view)` spawns a clipping container in one bundle: the node, its style,
`ClipChildren` and an `Interactable` so drags and the wheel over its empty space reach it. Clips nest: a child
container's clip rect is the intersection of every clipping ancestor, and hit testing uses the same rects, so a row
scrolled out of view cannot be clicked.

Settings on `ScrollView` (all have defaults that keep the earlier behavior):

| Field | Values | Effect |
|---|---|---|
| `axes` | `Both` (default), `Vertical`, `Horizontal` | The axes the view scrolls. A delta on the other axis passes to the enclosing view (a carousel inside a page). `ScrollView::vertical()` and `ScrollView::horizontal()` build these. |
| `overscroll` | `Clamp` (default), `Bounce` | `Bounce`: a drag pulls the content past its edge through a rubber band and a fling runs past it; a critically damped spring brings it back without overshooting into the content. |
| `chaining` | `Chain` (default), `Contain` | `Contain` keeps every delta inside the view: scrolling it never moves the views around it. |
| `inertia` | seconds (default 0.18) | Fling time constant. A fling at `v` px/s coasts `v * inertia` px if it runs until the velocity is zero and nothing blocks it. Motion also stops under 4 px/s, about `4 * inertia` px sooner, and a view stops at an edge. 0 turns flings off. |
| `wheel_smoothing` | seconds (default 0) | The wheel eases toward its target instead of jumping. Notches during the easing add up. |

## Input

- **Wheel.** It goes to the innermost scroll view under the pointer (a hovered scrollbar counts as its view). That
  view takes what fits on its axes and the rest passes outward. With no pointer (an agent or a test) it goes to the
  views around the focused node, and with no focus either, to every view as before ADR-520. A wheel notch never
  flings and never pulls past an edge.
- **Drag.** A drag on any node inside a scroll view moves the innermost view around it, and chains the same way.
  The content follows the pointer. A drag that starts on a scrollbar part is the scrollbar's.
- **Fling.** At release, the fling velocity is the mean drag velocity over the last 100 ms of the gesture. A finger
  that rests before lifting does not fling. Inertia is integrated exactly, so the distance does not depend on the
  frame rate.
- **Press.** Pressing inside a moving view stops its fling or spring. Releasing without a drag does not restart it.
- **Focus.** Focus moving to a node inside a view scrolls it into view on both axes, once per focus move.

## Scrollbar styles

`Scrollbars::styled(ScrollbarStyle::…)` (or `Scrollbars::overlay()` / `Scrollbars::classic()`) builds a named look.
Every preset covers both axes; a bar shows only when its axis overflows.

| Style | Look |
|---|---|
| `overlay` | The default design's bar: 10 px, 14 px under the pointer, pill-shaped, no track. The thumb is the text color at 25 % (40 % hovered, 55 % pressed). It hides after one idle second. |
| `compact` | The overlay bar at compact density: 8 px, 10 px under the pointer. |
| `classic` | Always visible, 12 px, square, with a track. |
| `hidden` | No bars. The view still scrolls by wheel, drag, keys and focus. |

The new `Scrollbars` fields are `hover_thickness`, `rounded`, `thumb_hover_color` and `thumb_active_color`.
`Scrollbars::default()` is unchanged.

## Performance

A frame where only scroll offsets moved does not run the flexbox pass. Layout walks the cached result again, at
the new offsets: positions and clip rects follow, and nothing is reconciled, measured or computed. Rows outside the
clip produce no draw quads. `UiLayoutStats` counts the runs (`full_runs`, `incremental_runs`, `scroll_runs`) and
the nodes the last run walked and changed. The 10,000-row test scrolls a plain container with no full or
incremental layout. For 10,000 items with only the visible rows alive, use a virtualised `ListView`.

## Method

`ui.scroll { entity, action?, ... }` where `entity` is the node id from `ui.query` (`node` works as an alias). It is
mutating (permission `edit_scene`).

- `action: "state"` (default) returns `{ entity, offset, max, viewport, content, velocity, overscrolled, axes,
  overscroll, chaining, inertia, wheel_smoothing, dragging, held, flings, settled, scrollbars, chain }`.
  `overscrolled` is how far each axis sits past its edge. `scrollbars` is the preset name, `custom` or null. `chain`
  lists the scroll views around the entity, innermost first. `settled` is true when no fling, drag, wheel easing or
  overscroll is left.
- `action: "wheel"` with `delta: [x, y]` scrolls like a wheel over the view: it fills first, then chains outward. It
  stops any motion on the chain. The answer adds `moved: [{ entity, offset }]` for each view of the chain.
- `action: "fling"` with `velocity: [x, y]` (px/s) starts a fling. The velocity is limited to the view's axes and
  to 12,000 px/s. The answer adds `distance: [x, y]`, `velocity * inertia` on each axis: the coast if
  the view ran until the velocity was zero and nothing blocked it. The view may stop sooner at an edge,
  or once speed falls under 4 px/s (about `4 * inertia` px short of `distance`). Poll `state` until
  `settled` is true.
- `action: "stop"` ends the motion and moves an overscrolled view back onto its edge.
- `action: "style"` with `style`: `overlay`, `compact`, `classic`, `hidden`, or `none` to remove the bars. The view
  needs a parent node, because the bars are its siblings.
- `action: "config"` with any of `axes`, `overscroll`, `chaining`, `inertia`, `wheel_smoothing` edits the view.

Error codes:

| Code | When |
|---|---|
| `unknown_entity` | The entity does not exist. |
| `not_scrollable` | The entity is not a `ScrollView`. |
| `unknown_scroll_style` | `style` names no preset. |
| `no_bar_parent` | `style` on a view without a parent node. |

Bad or missing parameters return `F0002`.
