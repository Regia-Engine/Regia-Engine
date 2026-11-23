# Camera framework (data-driven cameras)

A camera entity with a `CameraDriver` runs a `.camera.ron` asset every frame (spec 35, ADR-1577,
ADR-1637). The asset describes camera behavior as data: **rigs** (ordered pipelines of nodes),
**transitions** between rigs, a **director** that picks the active rig, **parameters** (the asset's
public inputs), **variables** and **input slots** that gameplay feeds, and **layers** (overlays and
additive rigs) for temporary and additive effects. The legacy `CameraRig` keeps working; a driver on
an entity turns the legacy update off for that entity only.

`CameraPlugin` installs everything (`CameraAssets`, `CameraHooks`, the `CameraLayerEvent` message and
the driver system in `PostUpdate`, `CameraSet::Update`). A driven camera feels the `CameraDirector`'s
shakes like any cinematic camera.

| Method | Permission | What it does |
|---|---|---|
| `camera.assets` | read | Every loaded asset: name, path, rigs, parameters, build status (`ok`, `warnings`, `errors`, `stale`), issue count. |
| `camera.validate` | read | Validate an asset's working copy; issues carry a stable `code`, `severity`, `rig`, `node` and text. |
| `camera.inspect` | read | A driver's active rigs with weights and a node-by-node pose trace, transition, layers, variables, input slots, skipped-node counters and the recent layer events. |
| `camera.set_var` | play | Write a runtime variable (matching type). |
| `camera.set_param` | play | Override an asset parameter for this driver (matching type). |
| `camera.set_input` | play | Write an input slot's raw per-frame value. The value is held (like a stick position) and integrated every frame until the next write. |
| `camera.reset_input` | play | Release an input slot: clear its held raw value and accumulated angles (`camera_unknown_name` for an unknown slot). |
| `camera.push_overlay` | play | Push an overlay layer; returns its id. Optional `context` (an entity name, or a world point `[x, y, z]`) is what the layer's `DriveTarget` follows instead of the driver's target; a different context is a separate layer. |
| `camera.add_additive` | play | Add an additive layer sorted by `order`; returns its id. Optional `context` as for `camera.push_overlay`. |
| `camera.release_layer` | play | Release a layer by `id`, or every layer of a `rig` (with `context`, only those layers with that context). |
| `camera.hold_overlay` | play | Stop a layer updating (its last pose is held). |
| `camera.set_driver` | edit_scene | Attach a driver to an entity, or change its asset, target or enabled flag. |
| `camera.load` | edit_scene | Read a `.camera.ron` under `assets/` into the registry: its source text, else its cooked entry. |

All answer `UNSUPPORTED` (`F0016`) in an app without `CameraPlugin` or `CameraAssetsPlugin`. The legacy `camera.get` and
`camera.set_mode` stay (see the framework page).

## Loading a file, and reloading it

`camera.load { path }` (or `load_camera_file(world, path)` from `regia_framework::camera::load`) reads the file at an asset
path, relative to `assets/`, and stores the asset under the name written inside it. A dev tree reads the source text; a
packaged game, which has the file in its pak (`regia cook` cooks it and fails on validation errors), reads the cooked text.
The answer is `{ path, asset, status, cooked, generation, issues }`. `camera.assets` also lists `files`, the file records
with what the last read did: `{ path, asset, cooked, generation, error: { reason, detail } }`.

A level names a camera with the reflected `CameraAssetSource { path, target, seed }` component (`target` is the `Name` of
the entity to follow, empty for none). `CameraPlugin` reads the file and attaches a `CameraDriver`, so a camera placed in a
level survives a play-in-editor snapshot, which a `CameraDriver` by itself does not.

With a watcher running (the editor, or a `dev-assets` game), a changed file is read again with no call:

| What the file now holds | What happens |
|---|---|
| A good asset | A new revision. Every driver on it rebinds on its next frame and keeps its variables and parameter overrides; its rigs restart. |
| An asset with validation errors | It becomes the working copy (`camera.validate` lists the issues). Drivers stay on the last good version. |
| Text that does not parse, or a newer version | Nothing changes. The file's record has `error.reason` `camera_asset_invalid` and the line and column in `error.detail`. |
| Nothing (the file is gone) | Nothing changes. The record has `camera_file_not_found`. |

A file that names a different asset than before stores a new asset; a `CameraAssetSource` driver moves to it, and the old
asset stays in the registry for drivers that name it. An asset a host stored itself with `CameraAssets::load_ron` and a path
follows that file as well. `CameraAssetsPlugin` alone gives a world the registry and the reload with no driver running: the
editor's edit world uses it, and its Cameras panel is the page for all of this
([gameplay panels](editor-gameplay-panels.md)).

## The asset

```ron
(
    format_version: 1,
    name: "third_person",
    params: { "boom_length": Float(4.5), "shoulder": Float(0.4), "aiming": Bool(false) },
    inputs: [ (name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-80.0, 30.0)) ],
    rigs: [
        (name: "explore", tags: ["ground"], root: [
            Pivot(target: DriveTarget, offset: (0.0, 1.5, 0.0)),
            LookInput(slot: "look"),
            Boom(length: Param("boom_length"), side_offset: Param("shoulder")),
            Collision(radius: 0.25, layers: ["static"], close_speed: 20.0, open_speed: 4.0),
            Fov(degrees: Literal(70.0)),
            SmoothPosition(depth: 8.0, side: 8.0, height: 6.0),
        ]),
        (name: "aim", tags: ["ground", "aim"], root: [ Fov(degrees: Literal(50.0)) ]),
    ],
    transitions: [
        (from: Any, to: Tag("aim"), blend: Smooth(0.25)),
        (from: Any, to: Any, blend: Smooth(0.5)),
    ],
    director: Rules(default: "explore", rules: [ (when: VarBool("aiming", true), rig: "aim") ]),
)
```

Unknown fields or variants are load errors that name the line and column. The format is `camera`
version 2 (v2 adds the `Rail` and `Crane` nodes, [rail and crane](camera-rail-crane.md)); a v1 file
like the one above loads and migrates, and a newer file is refused with an "upgrade the engine" error. Four starter assets that
reproduce the legacy modes ship in `assets/camera/presets/`: `third_person`, `overhead`,
`first_person`, `free` (also `regia_framework::camera::presets`).

Values: `Literal(x)`, `Param("name")` (an asset parameter; a driver override wins) or `Var("name")`
(a runtime variable re-read every frame). A variable is valid when it is declared in `variables:` or
when a parameter of that name seeds one.

### Nodes

A rig's nodes run in listed order on a pose seeded from the rig's previous final pose. Angles are
radians, yaw about +Y (0 faces −Z, positive turns toward −X), positive pitch looks up; rotation is
`Ry(yaw)·Rx(pitch)·Rz(roll)`. Roll is rebuilt every frame (it starts at 0).

| Node | Behavior |
|---|---|
| `Pivot` | Position = target + offset (`space: Target` rotates the offset; `World` does not); `aim` = that point plus `aim_offset`; `inherit_rotation` takes the target's rotation. A missing target keeps the pose and warns once. |
| `LookInput` | Yaw and pitch from an input slot's accumulated angles; `additive` composes them onto the current rotation. |
| `Boom` | Moves back along the view by `length` (eased at `length_speed` m/s, 0 = instant), shifts by `side_offset`, `up_offset`, and looks at `aim`. |
| `Collision` | Sphere-casts from the pivot to the position; the distance shortens to the hit minus a 5 cm skin, clamped so the probe centre stays on the near side of the hit (it may be 0, under the radius, and under 0.1 m). It closes at `close_speed` and reopens at `open_speed` (or exponentially at `open_rate`). No physics world: no effect. `reuse_last_frame` uses the previous frame's cast. |
| `Fov` | Vertical field of view in degrees, clamped to 1..=170. |
| `Offset` | Adds a translation and a rotation in degrees. |
| `SmoothPosition` | Per-axis exponential smoothing, `p += (in − p)(1 − exp(−k·dt))`; `k <= 0` = no smoothing. |
| `SmoothRotation` | The same for angles (shortest path for yaw); equal rates smooth the orientation as a whole. |
| `Lean` | Roll proportional to the look yaw's velocity, relaxed at `speed`. |
| `Frame` | Rotates minimally so a target stays in the soft zone; nothing moves inside the dead zone. |
| `Shake` | Smooth-noise shake from the driver seed, decaying at `decay`. |
| `Fly` | Free flight: a smoothed velocity from a move slot, `rise` and `sprint` values. |
| `Rail` / `Crane` | Ride a spline like a dolly / hang the camera from an arm (format v2; see [rail and crane](camera-rail-crane.md)). |
| `Group` / `When` | Run nested nodes when `enabled` / when `cond` holds. |

A node with an invalid run-time value (NaN, negative length, a missing slot) is skipped for the frame
and counted in `skipped`; the pose stays finite.

Conditions: `Elapsed(s)`, `VarBool(name, b)`, `VarCompare(name, Lt|Le|Eq|Ge|Gt, x)`, `HasTag(tag)`,
`RigIs(name)`, `All([..])`, `Any([..])`, `Not(..)`.

### Directors and transitions

`Fixed(rig)`; `Rules(default, rules)` (first matching rule wins, an empty `default` means "no
opinion"); `Ranked([(rank, director)])` (highest rank with a result); `Hook(name)` (a Rust function
registered with `CameraHooks`). A director may request several rigs; the first is the primary and the
others run as additive layers.

A rig change picks the transition from the outgoing rig's own list, then the incoming rig's, then the
asset's shared list (first match of `from`/`to`/conditions); no match is a cut. `blend`: `Cut`,
`Linear(t)`, `Smooth(t)`, `EaseIn(t)`, `EaseOut(t)`. `start_facing`: `OwnSeed`, `TargetFacing`,
`CarryOver`, `ContinueAim`. `hold_outgoing` freezes the outgoing rig; `share_state` merges the node
states of same-structure rigs (different structure warns once and cross-fades); `interruptible: false`
defers a new primary until the blend ends.

### Layers

- **Overlay** (`camera.push_overlay`): blends in over the pose beneath and removes it. It ends when
  released, when a later overlay has fully blended in, or by its rig's own transition with an
  `Elapsed` condition. Without `transition` it is a cut.
- **Additive** (`camera.add_additive`): adds its delta against the pose beneath, sorted by `order`;
  a duplicate `order` is refused unless `allow_same_order`.
- `camera.release_layer`: `instant` drops at once, otherwise the layer fades over its rig's
  `blend_out` (default 0.2 s).

`CameraLayerEvent { driver, id, rig, kind }` messages (`Entered`, `Settled`, `Leaving`, `Gone`, `Cut`)
come out in evaluation order; `camera.inspect` lists the last sixteen.

## Variables, parameters and input slots

Variables (`Bool`, `Float`, `Vec3`, `YawPitch`) are written by gameplay (`camera.set_var`). While
several rigs are active, each sees the table blended by its weight; a variable marked `local_only`
is never blended. Parameters keep their declared type: `camera.set_param` with the wrong type is
`camera_bad_value`. An input slot (`Axis1` or `Axis2`) integrates `value · speed · dt` into an
accumulated angle per axis, clamps pitch, wraps yaw, and can normalize, invert pitch or pull toward a
`YawPitch` variable (snap-behind-the-player). A positive horizontal value looks right.

## Debug

The `camera_rig` gizmo group (off by default; `debug.set_gizmos`) draws each active rig's pose
frustum tinted by weight, the boom line, the collision probe (red when blocked) and the aim point.

## Determinism

The pose is a pure function of the asset, parameters, variables, input values, target transforms,
physics query results, `dt`, the driver seed and the previous frame's state. No wall clock, no thread
order. A recorded stream of variables, inputs and target transforms replays to the same poses.

## Errors

Every error carries `data.reason`, one of these lowercase codes:

| Code | When |
|---|---|
| `camera_no_driver` | The entity has no `CameraDriver` (attach one with `camera.set_driver`). |
| `camera_unknown_asset` | No loaded asset has that name. |
| `camera_unknown_rig` | The asset has no rig with that name. |
| `camera_unknown_hook` | The asset's director names a hook nobody registered. |
| `camera_unknown_name` | The variable, parameter or input slot is not declared. |
| `camera_bad_value` | Wrong type, not finite, or out of range (including an unknown blend name). |
| `camera_unknown_layer_id` | No live layer has that id. |
| `camera_order_taken` | An additive layer already uses that `order`. |
| `camera_bad_path` | `camera.load`: not a plain path to a `.camera.ron` under `assets/`. |
| `camera_file_not_found` | `camera.load`: no source file and no cooked entry at that path. |
| `camera_asset_invalid` | The asset has validation errors and no earlier good version, or the driver is not bound to an asset; `data.issues` lists them. |

Validation issue codes (from `camera.validate`): `camera_unknown_rig`, `camera_unknown_name`,
`camera_duplicate_rig`, `camera_bad_value`, `camera_empty_rig` (warning), `camera_no_default_rig`,
`camera_share_structure` (warning), `camera_unknown_slot`, `camera_unused_param` (warning),
`camera_unknown_hook`. An asset with errors never replaces the previous good version in a running
driver.

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer. Absolute project paths and messages containing them are elided with `... /* comment */`.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `camera.add_additive`

Add an additive layer sorted by order: its pose delta is added to the pose beneath. A taken order is refused unless allow_same_order.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `rig` | string | yes | The rig name. |
| `order` | integer | yes | Sort order, lowest first. |
| `allow_same_order` | boolean | no | Allow a duplicate order. |
| `context` | any | no | What DriveTarget means for the layer: an entity name (string) or a world point [x, y, z]; absent keeps the driver target. |

`allow_same_order` defaults to false. `order` must fit a signed 32-bit integer.

Result: `{ id }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_no_driver`, `camera_order_taken`, `camera_unknown_rig`.

```json
camera.add_additive {"entity": 4294967300, "rig": "bump", "order": 3}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.add_additive -->

### `camera.assets`

Every loaded camera asset (.camera.ron): name, path, rigs, parameters, build status (ok | warnings | errors | stale) and issue count.

*Read, does not mutate.*

No parameters.

Result: `{ count, assets: [{ name, path, rigs, params, status, issue_count, active, revision }], files: [{ path, asset, cooked, generation, error: { reason, detail } }] }`.

Errors besides the universal codes: `F0016`.

```json
camera.assets {}
→ { "assets": [], "count": 0, "files": [] }
```

<!-- capture:camera.assets -->

### `camera.hold_overlay`

Stop a layer updating; its last pose is held.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `id` | integer | yes | The layer id. |

Result: `{ ok }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_no_driver`, `camera_unknown_layer_id`.

```json
camera.hold_overlay {"entity": 4294967300, "id": 1}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.hold_overlay -->

### `camera.inspect`

A camera driver's state: asset, the active rigs with weights and a node-by-node pose trace, the running transition, layers, variables, parameter overrides, input slots, skipped-node counters and the recent layer events.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |

Result: `{ entity, asset, enabled, bound, target, primary, time, pose, rigs: [{ name, weight, elapsed, skipped, pose, trace: [{ index, node, skipped, position, yaw, pitch, roll, fov_deg }] }], transition, layers, variables, param_overrides, inputs, events, skipped }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_no_driver`.

```json
camera.inspect {"entity": 4294967300}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.inspect -->

### `camera.load`

Read a .camera.ron under assets/ into the camera registry: its source text, else its cooked entry. Validation errors are reported in issues and drivers stay on the last good version; a changed file is read again by itself.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | yes | The file, relative to assets/. |

Result: `{ path, asset, status, cooked, generation, issues: [{ code, severity, rig, node, message }] }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_bad_path`, `camera_file_not_found`.

```json
camera.load {"path": "camera/player.camera.ron"}
```

```json,ignore
→ {
  "message": ... /* capture project path or message containing it */,
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "the path is relative to assets/; assets.list shows what exists",
    "reason": "camera_file_not_found"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.load -->

### `camera.push_overlay`

Push an overlay layer: the rig blends in over the director's pose and removes what is beneath. Returns the layer id; the same rig already an overlay is reused unless always_new.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `rig` | string | yes | The rig name. |
| `transition` | object | no | { blend: cut / linear / smooth / ease_in / ease_out, seconds }; absent is a cut. |
| `always_new` | boolean | no | Always start a new layer. |
| `context` | any | no | What DriveTarget means for the layer: an entity name (string) or a world point [x, y, z]; absent keeps the driver target. |

`always_new` defaults to false. An omitted transition is a cut. Within a supplied transition, `blend` defaults to `smooth` and `seconds` to 0.25; seconds must be finite and nonnegative.

Result: `{ id }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_bad_value`, `camera_no_driver`, `camera_unknown_rig`.

```json
camera.push_overlay {"entity": 4294967300, "rig": "aim", "transition": {"blend": "smooth", "seconds": 0.25}}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.push_overlay -->

### `camera.release_layer`

Release a layer by id, or every layer of a rig. Blended releases fade over the rig's blend-out time (default 0.2 s); instant drops at once.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `id` | integer | no | The layer id. |
| `rig` | string | no | A rig name (when no id). |
| `instant` | boolean | no | Drop at once. |
| `context` | any | no | With `rig`, only release the layers carrying this context (an entity name string or a world point [x, y, z]); without it, every layer of the rig is released. |

`instant` defaults to false. Supply `id` (u32) or `rig`; id takes precedence when both are given.

Result: `{ released }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_no_driver`, `camera_unknown_layer_id`, `camera_unknown_rig`.

```json
camera.release_layer {"entity": 4294967300, "id": 1, "instant": false}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.release_layer -->

### `camera.reset_input`

Release an input slot: clear the raw value camera.set_input holds and the slot's accumulated angles.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `slot` | string | yes | The input slot name. |

Result: `{ ok }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_no_driver`, `camera_unknown_name`.

```json
camera.reset_input {"entity": 4294967300, "slot": "look"}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.reset_input -->

### `camera.set_driver`

Attach a CameraDriver to a camera entity, or change its asset, target entity or enabled flag. Returns the driver summary.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `asset` | string | no | The asset name from camera.assets. |
| `target` | integer | no | The target entity id, or null to clear. |
| `enabled` | boolean | no | Evaluate or pause the driver. |

Omitted `asset`, `target` and `enabled` keep the existing driver’s values. Explicit `target: null` clears the target. A new driver requires an asset and starts enabled with no target unless those fields are supplied.

Result: `{ entity, asset, target, enabled, local_player, bound, status }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_no_driver`, `camera_unknown_asset`, `camera_unknown_hook`.

```json
camera.set_driver {"entity": 4294967300, "asset": "third_person", "enabled": true}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.set_driver -->

### `camera.set_input`

Write an input slot's raw per-frame value: a number (Axis1) or [x, y] (Axis2). The slot integrates it each frame.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `slot` | string | yes | The input slot name. |
| `value` | any | yes | A number or [x, y]. |

Result: `{ ok }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_bad_value`, `camera_no_driver`, `camera_unknown_name`.

```json
camera.set_input {"entity": 4294967300, "slot": "look", "value": [0.5, 0.0]}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.set_input -->

### `camera.set_param`

Override an asset parameter by name for this driver; the value must have the parameter's type.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `name` | string | yes | The parameter name. |
| `value` | any | yes | The new value. |

Result: `{ ok }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_bad_value`, `camera_no_driver`, `camera_unknown_name`.

```json
camera.set_param {"entity": 4294967300, "name": "boom_length", "value": 5.0}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.set_param -->

### `camera.set_var`

Write a runtime variable of the driver's asset (bool, number, [x, y, z] or { yaw, pitch } or [yaw, pitch], matching the variable's type).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `entity` | integer | yes | The camera entity with the driver. |
| `name` | string | yes | The variable name. |
| `value` | any | yes | The new value. |

Result: `{ ok }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_asset_invalid`, `camera_bad_value`, `camera_no_driver`, `camera_unknown_name`.

```json
camera.set_var {"entity": 4294967300, "name": "aiming", "value": true}
```

```json,ignore
→ { "message": "entity 4294967300 does not exist", "code": -32001, "data": { "code": "F0003", "hint": "scene.query_entities lists live entities" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.set_var -->

### `camera.validate`

Validate a camera asset's working copy and list the issues with stable codes (camera_unknown_rig, camera_unknown_name, camera_duplicate_rig, camera_bad_value, camera_empty_rig, camera_no_default_rig, camera_share_structure, camera_unknown_slot, camera_unused_param, camera_unknown_hook).

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `asset` | string | yes | The asset name from camera.assets. |

Result: `{ asset, status, issues: [{ code, severity, rig, node, message }] }`.

Errors besides the universal codes: `F0003`, `F0016`. `data.reason`: `camera_unknown_asset`.

```json
camera.validate {"asset": "third_person"}
```

```json,ignore
→ {
  "message": "no camera asset named \"third_person\"",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "camera.assets lists the loaded assets",
    "reason": "camera_unknown_asset"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:camera.validate -->

## Workflow: load a camera asset and drive one variable

1. `camera.assets` to see what is already loaded and which files have issues.
2. `camera.load` with an existing `path` under `assets/` ending in `.camera.ron`. `camera_bad_path` and `camera_file_not_found` are the path failures. Keep the returned asset name.
3. `camera.validate` on that asset name. Resolve validation issues before binding it; an invalid working copy can leave an earlier good revision active.
4. `camera.set_driver` to attach that asset to a camera entity. `camera_no_driver` on later calls means the entity has none.
5. `camera.set_var` or `camera.set_param` for the value gameplay feeds, then `camera.inspect` and read the active rig, the weights and the node trace.
6. `camera.push_overlay` for a temporary rig, `camera.hold_overlay` to freeze it, `camera.release_layer` when it should come off. `camera_unknown_layer_id` means the id was already released.

<!-- /depth-2:method-reference -->

## Blends and one director per player

`CameraDirector::blend_to(target, duration, curve)` moves cameras from where they are to a target
pose, or to an entity's transform that they then follow, over `duration` seconds along an `Easing`
`curve` (spec 14 § B5, ADR-7203). The pose is the target exactly at `duration`; a pose target holds
until `clear_blend` or the next blend. Blends run in `PostUpdate` after the rigs and the cinematic
players and before the shakes, so a shake still shows on a blended camera.

The `CameraDirector` resource is the shared director: its blends move every `CameraRig` or cinematic
camera without a `PlayerCamera`. Each local player has its own director on the `PlayerManager`
(`camera_director_mut(index)`, created when the player joins, dropped when they leave); it moves the
cameras marked `PlayerCamera(index)`, so two split-screen players blend independently. Each player's
director also owns its shakes (ADR-7263): a `PlayerCamera(index)` camera feels only that player's
director (through its shake sensor, which it has by default like a rig), and the shared director's
shakes reach only the cameras that belong to no player. Route a shake to one player with
`PlayerManager::camera_director_mut(index).shake(...)` or `camera.shake { "player": index }`
(book/agents/camera-shake.md); a `ShakeEmit` still reaches every listening camera.

```json
{ "method": "camera.blend_to", "params": { "translation": [0.0, 3.0, 8.0], "duration": 1.5, "curve": "sine_in_out" } }
```

answers `{ player, duration, curve, target_entity }`. `target_entity` (an entity id) blends to that
entity instead of a pose; `rotation` is a quaternion `[x, y, z, w]` (default identity); `player` picks
a local player's director. `duration: 0` cuts. `camera.get` reports the shared blend's time fraction
as `blend` (`null` without a blend); wait for it with `play.wait_until` on `camera.get` `blend == 1`.
`camera.get` also lists each local player's director as `players: [{ player, shaking, shakes, strength,
blend }]` (`shakes`: running shakes and mount oscillations, `strength`: the strongest envelope now).
Errors: an unknown `curve`, a missing target or a bad `duration` are `invalid_params`; a `player` that
is not local, or a `target_entity` that does not exist, is `NOT_FOUND`.
