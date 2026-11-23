# Product viewer drag view

The product viewer (the light web player and the Android viewer, ADR-2540) shows one piece and lets
the user turn it. Both players use the same camera controller, crate `regia_viewer_view`, so a drag
feels the same in a browser and on a phone.

| Input | Does |
|---|---|
| one finger, or the primary mouse button | turn the view (drag right: the piece turns right) |
| lifting after a quick drag | keep turning, slowing down (fling) |
| two fingers | pan by the midpoint, zoom by the spread, twist about the front axis where `spin` allows |
| secondary or middle mouse button | pan |
| wheel or trackpad scroll | zoom |
| double tap or double click | animate back to the default view |

## Limits come from the recipe

The recipe's `view_range` is binding (interfaces.md § 3). The controller clamps every input to it,
and so can never show a view outside it:

| Field | Default | Limit |
|---|---|---|
| `front_axis` | `[0, 0, 1]` | the default view looks down it |
| `tilt_max_deg` | 30 | the angle between the view and the front axis, 0 to 89 |
| `pan_bounds_mm` | `[[-3, 3], [-3, 3]]` | the look-at point's offset in the front plane (x right, y up) |
| `spin` | `none` | `none`: tilt only; `rock`: the horizontal limit widens to `rock_max_deg` (the vertical stays `tilt_max_deg`); `about_front_axis`: plus a two-finger twist about the front axis; `free`: a full orbit (the vertical stops at 85 degrees) |
| `max_px_per_mm` | 400 | the closest zoom, in screen pixels per mm of the piece, for the current viewport |

`camera_default` (`distance_mm` 60, `focal_length_mm` 100, `sensor_mm` 36) frames zoom 1. Zoom narrows
the lens and never moves the camera, so the camera cannot enter the piece.

A malformed or out-of-range `view_range` is refused with a code (below), never widened.

## How it stays smooth and cheap

- The controller keeps a *target* view (where input put it) and a *shown* view (what is drawn). Each
  frame the shown view moves toward the target by `s = t + (s - t) * exp(-dt / 0.07 s)`, the same at
  any frame rate.
- Every limit is convex (a disk or ellipse of tilt, a box of pan, an interval of zoom), so the shown
  view, which always lies between two in-range views, is in range too.
- `DragView::update(dt)` returns whether to draw. Once the view settles it returns false and a lazy
  loop stops drawing: zero draws at idle.
- The crate reads no clock, starts no thread and touches no file: the host passes timestamps.

## `viewer.view_simulate`

Runs a scripted input sequence through a fresh drag view and reports where it ends. Pure: it reads
no world state.

```json
{ "method": "viewer.view_simulate", "params": {
  "view_range": { "tilt_max_deg": 30, "spin": "none" },
  "viewport": [800, 600],
  "events": [
    { "down": { "id": 1, "x": 100, "y": 100, "t": 0 } },
    { "move": { "id": 1, "x": 700, "y": 100, "t": 16 } },
    { "up": { "id": 1, "t": 500 } },
    { "settle": true },
    { "frames": [10, 16.7] }
  ] } }
```

Answer (abridged, from the run recorded in the lane report): the long drag stops at the cone
(`view_angle_deg` 30), the view settles after 44 frames, and the 10 idle frames after it draw nothing
(`draws` 44 of 54 `frames`):

```json
{ "target": { "tilt_deg": [30.0, 0.0], "spin_deg": 0.0, "pan_mm": [0.0, 0.0], "zoom": 1.0 },
  "pose": { "eye_mm": [-30.0, 0.0, 51.96], "look_at_mm": [0, 0, 0], "up": [0, 1, 0],
            "fov_y_deg": 20.41, "fov_short_deg": 20.41 },
  "view_angle_deg": 30.0, "widest_angle_deg": 30.0, "max_view_angle_deg": 30.0, "zoom_max": 14.4,
  "frames": 54, "draws": 44, "animating": false, "spin": "none",
  "build_camera": { "rig_preset": "tilt", "tilt_deg": 30.0, "tilt_xy_deg": [30.0, 0.0], "zoom": 1.0,
                    "focal_length_mm": 100.0, "sensor_mm": 36.0, "focus_distance_mm": 60.0, "f_stop": 11.0,
                    "dof": false } }
```

Events: `down {id, x, y, t, pointer?}` (`pointer`: `touch`, `mouse`, `mouse_secondary`),
`move {id, x, y, t}`, `up {id, t}`, `cancel {id}`, `wheel <lines>`, `frame <dt_ms>`,
`frames [count, dt_ms]`, `set_view {tilt_deg?, spin_deg?, pan_mm?, zoom?, immediate?}`,
`reset true`, `settle true` (60 Hz frames until idle, at most 10 s). One script runs at most
1000000 frames. Pass `recipe` (a whole recipe)
instead of `view_range` and `camera_default` to use a product's own limits.

`build_camera` is the `camera` block of a `jewelry.build/1` for the target view: `tilt_deg` is the
cone angle; the optional `tilt_xy_deg` keeps its direction.

The capability marker `viewer_view_pack_v1` says the method exists.

## Errors

All answer `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `view_bad_event` | an event is not a one-key object of a known kind, or lacks `id`, `x` or `y`; a `viewport` that is not two positive numbers; a `set_view` field that is present but not a finite number (`tilt_deg` and `pan_mm` are `[x, y]`, `immediate` is a bool) |
| `view_too_many_events` | more than 65536 events (the dispatcher caps any array parameter there too), or more than 1 000 000 frames stepped by the whole script (`frame`, `frames` and `settle` together, ADR-5597; the event that would pass it is refused before it runs) |
| `view_range_bad_value` | a field has the wrong type or is not finite |
| `view_range_bad_axis` | `front_axis` has zero length |
| `view_range_bad_tilt` | `tilt_max_deg` outside 0 to 89 |
| `view_range_bad_pan` | a pan bound with min over max, or beyond 1000 mm |
| `view_range_bad_spin` | `spin` is not `none`, `rock`, `about_front_axis` or `free` |
| `view_range_bad_rock` | `spin` `rock` without `rock_max_deg`, or it is outside 0 to 180 |
| `view_range_bad_zoom` | `max_px_per_mm` is not positive |
| `view_range_bad_camera` | a `camera_default` value is not positive |

## For hosts (web player, Android viewer)

<!-- ignore-reason: a fragment of a host; the recipe, pointer events and clock come from the platform, and the book's snippet crate does not build the viewer crates -->
```rust,ignore
use regia_viewer_view::{DragView, PointerKind};

let mut view = DragView::from_recipe(&recipe)?;      // limits from view_range
view.controller.set_viewport(width_px, height_px);
view.controller.set_pivot(piece_bounds_center);
// pointer events, with the platform's own timestamps in ms
view.pointer_down(id, PointerKind::Touch, x, y, t_ms);
view.pointer_move(id, x, y, t_ms);
view.pointer_up(id, t_ms);
// once per frame
if view.update(dt_s) {
    let pose = view.pose();                          // eye, look-at, up, fov
    // draw with pose.view_matrix() and pose.projection(aspect)
}
if !view.is_animating() { /* stop requesting frames */ }
```

The fuzz test `crates/regia_viewer_view/tests/fuzz_limits.rs` drives random pointer and input
sequences (including NaN, infinities and huge values), random ranges, viewports and frame times,
and checks after every step, from the view matrix itself, that the cone, the pan box and the zoom
limit hold.
