# Splines and shapes

A **spline** is the `regia_scene::spline::Spline` component: control points in the entity's local
space joined by cubic Hermite segments. Roads, fences, walls, rivers and camera-free paths all start
from one (specs/64 § 2). A **shape** is the `regia_scene::shape::Shape` component: a box, sphere,
capsule or convex hull used as a region by generators and tools (not a collider; edit it with the
generic `scene.*` component methods).

## The model

| Field | Meaning |
|---|---|
| `points` | Control points: `position`, `tangent_in`, `tangent_out`, `mode`, `roll_deg`, `scale` (width, height factors, default 1, 1), `values` |
| `closed` | The last point joins the first |
| `up` | `rotation_minimizing` (default: the frame turns as little as possible, starting from world Y) or `world_y` (world up projected off the tangent) |
| `attrs` | Custom per-point values: `{ name, ty: f32 \| vec3 \| name }`; numbers and vectors interpolate linearly, names hold the start point's value |

Tangent modes: `auto` (centripetal Catmull-Rom through the neighbors, the curve camera paths use),
`linear` (straight toward the neighbor; a corner next to a curved point), `free` (both tangents as
stored), `aligned` (`tangent_out` as stored, `tangent_in` along it at its stored length), `step`
(the segment starting here stays at this point; for attribute tracks). Tangents are derivatives over
the segment parameter: a quarter circle of radius r from two `free` points uses tangents 1.6569·r
long.

Saved in scenes as `regia_scene::Spline` (`format_version` 1, check-formats name `spline`) and
`regia_scene::Shape` (`shape` v1).

## Read and sample

```json
spline.get { "entity": 4294967310 }
→ { "points": [{ "position": [0, 0, 0], "tangent_in": [0, 0, 0], "tangent_out": [0, 0, 0],
      "mode": "auto", "roll_deg": 0, "scale": [1, 1], "values": [] }, …],
    "closed": false, "up": "rotation_minimizing", "attrs": [], "length": 41.7,
    "segment_lengths": [20.3, 21.4] }

spline.sample { "entity": 4294967310, "count": 3 }
→ { "length": 41.7, "samples": [{ "position": […], "tangent": […], "up": […], "right": […],
      "roll_deg": 0, "scale": [1, 1], "curvature": 0.012, "along_m": 0, "key": 0, "values": [] }, …] }

spline.closest { "entity": 4294967310, "point": [12, 0, 3] }
→ { "key": 0.61, "distance_m": 2.4, "along_m": 12.9, "position": [12.4, 0, 0.6] }
```

`at: [meters…]` samples at given distances (clamped to the curve); `count: n` spreads n samples with
both ends included. `world: true` measures in world space through the entity's `GlobalTransform`
(non-uniform scale applies to positions and tangents). `key` is segment index plus the local
parameter. `right` is `tangent × up`; positive `roll_deg` turns up toward right.

## Write and edit

```json
spline.set { "entity": 4294967310, "closed": false,
             "attrs": [{ "name": "width", "ty": "f32" }],
             "points": [{ "position": [0, 0, 0], "values": [4] }, { "position": [30, 0, 10], "values": [6] }] }
→ { "length": 31.6 }

spline.edit { "entity": 4294967310, "ops": [
    { "op": "insert", "index": 1, "point": { "position": [15, 0, -4], "values": [5] } },
    { "op": "set_mode", "index": 0, "mode": "linear" },
    { "op": "move", "index": 2, "position": [32, 0, 12] },
    { "op": "set_value", "index": 1, "attr": "width", "value": 4.5 },
    { "op": "remove", "index": 3 } ] }
→ { "length": …, "points": […] }
```

`spline.set` adds a `Spline` to an entity that has none; a point may be a bare `[x, y, z]`. Both calls
validate the whole result first (finite numbers, at most 4096 points, values matching `attrs`); a
failed call changes nothing.

In the editor each call that changes a spline is one undo entry named `Edit spline`: `editor.undo` reverts it
(and removes the component if `spline.set` added it), `editor.redo` brings it back, `editor.history` lists it. A call that
writes the same values, or fails, adds no entry.

A change to a spline marks every generator that reads it (through `in.entities`, for example a road network) stale, so
the next run picks up the new curve (specs/62 § 5.3); the test `splines_are_captured_in_world_space_and_edits_mark_stale`
proves it.

## Error codes

Answered as `F0002` (`F0003` for `not_a_spline`) with `data.reason`:

| `data.reason` | When |
|---|---|
| `not_a_spline` | The entity does not exist or has no `Spline` (F0003) |
| `spline_index_out_of_range` | An edit op's `index` is past the points |
| `spline_attr_unknown` | `set_value` names an attribute not in `attrs` |
| `spline_too_many_points` | More than 4096 points |
| `spline_too_many_samples` | More than 100 000 samples in one `spline.sample` |
| `spline_invalid` | Malformed params: non-finite numbers, unknown mode or op, values not matching `attrs` |

## Using splines in procedural graphs

Spline and polygon nodes (`sample.spline`, `spline.*`, `polygon.*`, `spline.modules`) are listed by
`procgen.node_types` and described in [Procedural generators](procgen.md#splines-polygons-and-fences).
