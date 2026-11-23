# Eye gaze and per-instance bind poses: drive and verify

User guide: `book/user-guide/eyes-and-bind-poses.md`. These calls need `regia_animation::AnimationPlugin` and a
rig with an `Animator` and a `SkinnedMesh`. Entity ids come from `anim.list`.

## Gaze

`anim.gaze_set` points the eyes. It creates the `Gaze` from the skeleton's eye joints by name when the rig has none
(`INVALID_PARAMS` if no `eye_l`/`eye_r`-style joints exist). `target` is `[x, y, z]` (world), an entity id, or
`null` (eyes return to rest). Optional `weight`, `convergence`, `saccade_speed`.

```json
{"method": "anim.gaze_set", "params": {"entity": 4294967310, "target": [1.0, 1.7, -2.0]}}
→ { "entity": 4294967310, "target": { "point": [1, 1.7, -2] }, "weight": 1.0, "valid": false, ... }
```

`valid` is false until the first frame solves it. Step (`play.step`) and read `anim.gaze_get`:

```json
{"method": "anim.gaze_get", "params": {"entity": 4294967310}}
→ { "target": { "point": [1, 1.7, -2] }, "valid": true, "error": 0.0004,
    "left":  { "yaw": -0.31, "pitch": 0.04, "goal_yaw": -0.31, "goal_pitch": 0.04, "saturated": false,
               "channels": { "up": 0.09, "down": 0.0, "in": 0.5, "out": 0.0 } },
    "right": { ... }, "lids": 2, "morphs": 4, "head_follow": false }
```

`error` is the worst angle (radians) between an eye's gaze and the direction to the target; it is near 0 unless the
target is outside the limits (`saturated: true`) or the eyes are still moving (`yaw` against `goal_yaw`).
`channels` are the eyelid-follow / eye-look morph weights already multiplied by `weight`.

Verify: set a target, `play.step` about 10 frames, `anim.gaze_get` until `error < 0.01`, then `seeing.screenshot` and
compare to a frame with `target: null`.

## Bind poses

```json
{"method": "anim.bind_pose_set", "params": {"entity": 4294967310, "scale": 1.15, "bone_scales": {"calf_l": 1.1}}}
→ { "overridden": true, "joints": 60, "ratio_min": 1.15, "ratio_max": 1.265,
    "changed": [ { "joint": "calf_l", "ratio": 1.265 }, ... ] }
```

`scale` multiplies every bone length, `bone_scales` the bone leading to the named joint (it multiplies `scale`).
Every factor must be greater than 0 (0 collapses the bones, a negative factor mirrors them). A rig with its own
proportions aims its eyes from where they are drawn, and gaze runs before the rig drivers and spring bones, so
twist and corrective drivers and head-rooted springs follow the gazed head.
`{"clear": true}` removes the instance data. `anim.bind_pose_get` reports `overridden`, the ratio range and the
joints that differ. A rig whose instance joint count differs from the skeleton shows `matches_skeleton: false` and is
rendered with the shared bind pose.

Verify: set a scale, `play.step`, take a screenshot, compare with `scale: 1.0`; the character is taller or longer
with the same animation and the skin stays attached (no stretched triangles, no detached limbs).

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `anim.bind_pose_get`

Whether a rig has its own bind pose (per-instance proportions) and how its bone lengths differ from the shared skeleton.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | Rig entity id (from anim.list). |

Result: `{ entity, overridden, joints, matches_skeleton?, ratio_min?, ratio_max?, changed?: [{ joint, ratio }] }`.

Errors besides the universal codes: `F0003`.

```json
anim.bind_pose_get {"entity": 4294967310}
```

```json,ignore
→ { "message": "entity 4294967310 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "list animated rigs with anim.list" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:anim.bind_pose_get -->

### `anim.bind_pose_set`

Give one rig its own proportions over the shared skeleton and clips: a uniform bone scale, per-joint bone scales, or clear to go back to the shared bind pose.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | Rig entity id (from anim.list). |
| `scale` | number | no | Factor for every bone length; default 1 when only bone_scales is given. Finite and greater than 0. |
| `bone_scales` | object | no | { joint name: factor } for the bone leading to that joint; multiplies `scale`. Default empty; factors must be finite and greater than 0, and names must exist in the skeleton. |
| `clear` | boolean | no | Default false. True removes the instance bind pose and ignores the scale fields. |

Result: `The anim.bind_pose_get object after the change.`.

Errors besides the universal codes: `F0003`.

```json
anim.bind_pose_set {"entity": 4294967310, "scale": 1.15, "bone_scales": {"calf_l": 1.1}}
```

```json,ignore
→ { "message": "entity 4294967310 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "list animated rigs with anim.list" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:anim.bind_pose_set -->

### `anim.gaze_get`

A rig's eye gaze: target, per-eye yaw/pitch against the limits, saturation, the eyelid-follow morph channels and the aim error.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | Rig entity id (from anim.list). |

Result: `{ entity, target: null | { point } | { entity }, weight, valid, error, convergence, saccade_speed, min_distance, left, right: { yaw, pitch, goal_yaw, goal_pitch, saturated, channels: { up, down, in, out } }, lids, morphs, head_follow }`.

Errors besides the universal codes: `F0003`.

```json
anim.gaze_get {"entity": 4294967310}
```

```json,ignore
→ { "message": "entity 4294967310 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "list animated rigs with anim.list" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:anim.gaze_get -->

### `anim.gaze_set`

Point a rig's eyes at a world point or an entity (or null to rest); creates the gaze from the skeleton's eye joints when the rig has none. Also weight, convergence and saccade speed.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | Rig entity id (from anim.list). |
| `target` | any | no | [x, y, z] world point, an entity id, or null (eyes return to rest). Omitted keeps the current target; a newly created gaze starts at rest. |
| `weight` | number | no | Finite blend with the animation; clamped to 0..1. Omitted keeps the current value. |
| `convergence` | number | no | Finite, clamped to 0..1: 0 = parallel eyes, 1 = each eye aims at the target. Omitted keeps the current value. |
| `saccade_speed` | number | no | Finite eye speed in radians per second; 0 = instant. Omitted keeps the current value. |

Result: `The anim.gaze_get object after the change.`.

Errors besides the universal codes: `F0003`.

```json
anim.gaze_set {"entity": 4294967310, "target": [1.0, 1.7, -2.0]}
```

```json,ignore
→ { "message": "entity 4294967310 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "list animated rigs with anim.list" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:anim.gaze_set -->

## Workflow: aim the eyes, then restore the bind pose

1. Find a character with an Animator and SkinnedMesh, then call `anim.gaze_get`. `F0003` can mean a missing entity or gaze component; `anim.gaze_set` can create a gaze on a rig with recognizable eye joints.
2. `anim.gaze_set` the target position and weights, advance fixed frames with `play.step`, then read `anim.gaze_get` to check the weights and solved eye error.
3. `anim.bind_pose_get` for the joints the mesh was built with.
4. `anim.bind_pose_set` with bone_scales for the joints you mean to replace, then `anim.bind_pose_get` to confirm the changes.
5. Restore with `anim.bind_pose_set` and `clear: true`, and `anim.gaze_set` with `target: null`. Step and read both getters to confirm the shared bind pose and resting gaze.

<!-- /depth-2:method-reference -->
