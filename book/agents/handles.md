# Handles and snap targets

## `editor.snap_targets`

The snap points of an entity subtree. Params: `entity` (required), `kinds` (array of `face_center`,
`edge_midpoint`, `bounds_center`, `connector`, `all`; default all), `max_points` (default 1024, cap 8192).
Add `x`, `y` (pixels of a `width`×`height` pane, `view` as in `editor.pick`) and `radius` (default 10) to also get
`nearest`.

```json
{ "method": "editor.snap_targets", "params": { "entity": 4294967297, "kinds": ["face_center"], "x": 320, "y": 200 } }
```

Returns `count`, `truncated`, `points[]` (`kind`, `position`, `entity`, `normal` or null) and, with `x`/`y`,
`nearest` (a point or null). Check it worked: a box gives 6 `face_center` and 12 `edge_midpoint` points.

## `editor.handle_drag`

Replays a synthetic pointer drag through the handle state machine with no UI, so an agent can check handle math.
Params: `handles` (required; `{type: point, pos}`, `{type: arrow, origin, dir}`,
`{type: arc, center, axis, from, to, radius}`, `{type: plane, origin, normal, size}`; world space), `from` and `to`
(`[x, y]` pixels; the button goes down at `from`, moves in `steps` (default 8) and releases at `to`),
`snap_entity` + `kinds` (snap to that subtree), `radius`, `width`, `height`, `view`.

Returns `hovered` (handle under `from`), `grabbed`, `events[]` (`handle`, `phase` hover/begin/drag/end, `delta`,
`total`, `position`, `angle`, `angle_delta`, `snapped`) and `result` (the last event). `grabbed: false` means `from`
was not within the pick radius of any handle: aim at `handle.anchor` projected to the pane. Mutates nothing.
The interactive widget (`regia_editor::handles::handles`) is not reachable from the protocol; use
`editor.capture_viewport` to see the editor, and this method to test the math.

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `editor.handle_drag`

Replay a synthetic pointer drag through the custom-handle state machine (editor::handles) without a UI: press at `from` on a handle, move to `to`, release. Handles are {type: point, pos} | {type: arrow, origin, dir} | {type: arc, center, axis, from, to, radius} | {type: plane, origin, normal, size}, in world space. Returns the hovered handle, every begin/drag/end event (world delta, total, position, arc angle, snapped point) and the final result. Mutates nothing.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `handles` | array | yes | Handle descriptions (see above). |
| `from` | array | yes | [x, y] pixel where the button goes down. |
| `to` | array | yes | [x, y] pixel where it is released. |
| `steps` | number | no | Interpolated pointer moves between from and to. Default 8; clamped to 1..=240. |
| `snap_entity` | number | no | Snap to the points of this entity's subtree while dragging. |
| `kinds` | array | no | Snap kinds (as editor.snap_targets). Default all. |
| `radius` | number | no | Pick and snap radius in pixels. Default 10. |
| `width` | number | no | Pane width in pixels. Default 640; clamped to 64..=4096. |
| `height` | number | no | Pane height in pixels. Default 480; clamped to 64..=4096. |
| `view` | string | no | Pane camera: perspective (default, the editor camera), top, front or side. |

Omitted snap_entity uses no snap targets. Handle positions are three-number world vectors; arc angles and deltas are radians. From and to each require two pixel coordinates.

Result: `{ hovered, grabbed, result, events: [{ handle, phase, delta, total, position, angle, angle_delta, snapped }] }`.

Errors: `F0002` for malformed handles, coordinates, view or snap arguments.

```json
editor.handle_drag {"handles": [{"type": "point", "pos": [0, 1, 0]}], "from": [320, 240], "to": [360, 240], "steps": 2, "width": 640, "height": 480}
→ {
  "events": [
    {
      "angle": null,
      "angle_delta": 0.0,
      "delta": [
        0.0,
        0.0,
        0.0
      ],
      "handle": 0,
      "phase": "begin",
      "position": [
        0.0,
        1.0,
        0.0
      ],
      "snapped": null,
      "total": [
        0.0,
        0.0,
        0.0
      ]
    },
    {
      "angle": null,
      "angle_delta": 0.0,
      "delta": [
        0.3176751136779785,
        -2.86102294921875e-06,
        -0.2173314094543457
      ],
      "handle": 0,
      "phase": "drag",
      "position": [
        0.3176751136779785,
        0.9999971389770508,
        -0.2173314094543457
      ],
      "snapped": null,
      "total": [
        0.3176751136779785,
        -2.86102294921875e-06,
        -0.2173314094543457
      ]
    },
    {
      "angle": null,
      "angle_delta": 0.0,
      "delta": [
        0.3176767826080322,
        -8.821487426757812e-06,
        -0.2173299789428711
      ],
      "handle": 0,
      "phase": "drag",
      "position": [
        0.6353518962860107,
        0.999988317489624,
        -0.4346613883972168
      ],
      "snapped": null,
      "total": [
        0.6353518962860107,
        -1.1682510375976562e-05,
        -0.4346613883972168
      ]
    },
    {
      "angle": null,
      "angle_delta": 0.0,
      "delta": [
        0.0,
        0.0,
        0.0
      ],
      "handle": 0,
      "phase": "end",
      "position": [
        0.6353518962860107,
        0.999988317489624,
        -0.4346613883972168
      ],
      "snapped": null,
      "total": [
        0.6353518962860107,
        -1.1682510375976562e-05,
        -0.4346613883972168
      ]
    }
  ],
  "grabbed": true,
  "hovered": 0,
  "result": {
    "angle": null,
    "angle_delta": 0.0,
    "delta": [
      0.0,
      0.0,
      0.0
    ],
    "handle": 0,
    "phase": "end",
    "position": [
      0.6353518962860107,
      0.999988317489624,
      -0.4346613883972168
    ],
    "snapped": null,
    "total": [
      0.6353518962860107,
      -1.1682510375976562e-05,
      -0.4346613883972168
    ]
  }
}
```

<!-- capture:editor.handle_drag -->

### `editor.snap_targets`

The snap points of an entity and its descendants: face centers (coplanar triangles merged), feature-edge midpoints, bounding-box centers and SnapConnector contact faces. With x and y (pixels of the width×height pane) also returns the nearest point within radius pixels.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | number | yes | Entity id of the subtree root. |
| `kinds` | array | no | Any of face_center, edge_midpoint, bounds_center, connector, all. Default all. |
| `max_points` | number | no | Points returned at most (default 1024, cap 8192); truncated says if cut. |
| `x` | number | no | Pointer pixel x: with y, also return the nearest point. |
| `y` | number | no | Pointer pixel y. |
| `radius` | number | no | Snap radius in pixels. Default 10. |
| `width` | number | no | Pane width in pixels for x/y. Default 640; clamped to 64..=4096. |
| `height` | number | no | Pane height in pixels for x/y. Default 480; clamped to 64..=4096. |
| `view` | string | no | Pane camera: perspective (default, the editor camera), top, front or side. |

Only supplying both x and y requests nearest-point picking; otherwise the nearest field is omitted.

Result: `{ entity, count, truncated, points: [{ kind, position, entity, normal }], nearest? }`.

Errors besides the universal codes: `F0002`, `F0003`.

```json
editor.snap_targets {"entity": 1, "max_points": 4}
```

```json,ignore
→ { "message": "entity 1 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "use scene.query_entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:editor.snap_targets -->

## Workflow: snap a drag to a mesh face

1. `editor.snap_targets` with the `entity` id. Add `x`, `y`, `width` and `height` when you also want `nearest` under the pointer. An unknown entity is `F0003`.
2. Pick a point from `points` (a `face_center` or a `connector`).
3. `editor.handle_drag` with one handle (`type` `point` at that position), `from` and `to` in pixels, and `snap_entity` set to the same entity so the drag reports `snapped`.
4. Read `events` in order: begin, drag, end. `result` is the handle's final world value. The call does not move the entity.

<!-- /depth-2:method-reference -->
