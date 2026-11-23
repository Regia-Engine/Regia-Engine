# Animation editor

The editor's animation tooling has three panels — **Anim Graph**, **Clip** and **Skeleton** — and
an `editor.anim_*` agent surface that edits the same documents. Every mutating call is an undo
entry on the editor's shared stack (`editor.undo` / `editor.redo` revert it in order with level
edits).

Two documents are open at a time:

- the **graph** (`.animgraph.ron`, frozen format v3; v1 and v2 still load
  through the migrator) plus an editor-only `.animgraph.layout.ron` of canvas
  positions next to it (layout format v1);
- the **rig's authoring sidecar** (`<source>.animedit.ron`, format v1,
  `docs/formats/animedit-ron.md`): notifies, named curves, property tracks, the root-motion mode,
  sockets and IK chains authored on top of an imported glTF.

## Graph model

Nodes have numeric ids, a canvas `scope` and named input slots. The root canvas is `"root"`; a
state's sub-graph is `{ "machine": <state machine id>, "state": "Idle" }`. Slots are named like
the runtime's node paths: `a`/`b` (blend), `base`/`additive`, `base`/`layer0…` (layers), `inner`
(IK, look-at, root motion), `source` (montage slot). Each canvas has one **output**: `editor.anim_connect { child }`
without a `parent` makes `child` its canvas's output. Graphs are trees: connecting a node that
already feeds something moves it.

| Kind | `data` fields (serde JSON, shallow-merged) |
|---|---|
| `clip` | `clip`, `looping`, `speed`, `sync_group` |
| `blend_space_1d` | `space: { param, samples: [{ value, clip }] }`, `sync_group` |
| `blend_space_2d` | `space: { param_x, param_y, samples: [{ point: [x, y], clip }], triangles: [] }`, `sync_group` |
| `state_machine` | none — use `editor.anim_state` / `editor.anim_transition` (`set_node` accepts `{ initial }`) |
| `blend` | `alpha_param` |
| `additive` | `weight_param` |
| `layer` | `layers: [{ mask: { joints, include_children }, mode: "Override"\|"Additive", weight_param }]` |
| `two_bone_ik` | `chain: [root, mid, tip]`, `target: { "Param": name }\|{ "Socket": name }`, `pole`, `weight_param` |
| `look_at` | `joint`, `target`, `max_yaw`, `max_pitch`, `weight_param` |
| `root_motion` | `mode: "None"\|"XZ"\|"XYZ"\|"Yaw"` |
| `slot` | `name` (a slot of the skeleton's table), `eval_idle_source` (default `true`); input `source`. Plays that slot's montages inside the graph (`.animgraph.ron` v3, spec 34 M9): nodes above it (IK, layers) see the montage pose |
| `chain_ik` | `root`, `tip`, `solver: "Fabrik"\|"Ccd"`, `target`, `pole`, `limits: null\|{ "Uniform": limit }\|{ "PerJoint": [{ joint, limit }] }`, `iterations` (≤ 256), `tolerance` (meters), `weight_param`; input `inner`. FABRIK or CCD over a chain of any length (`.animgraph.ron` v4, ADR-8475) |
| `body_aim` | `joints: [[name, share], …]` (bottom first), `aim_joint`, `axis: [x, y, z]` (default `[0, 0, -1]`), `target`, `limits`, `passes` (1..=32, default 8), `weight_param`; input `inner`. Turns the stack so `aim_joint`'s axis points at the target (v4) |
| `stride_warp` | `ground_speed_param` (float, m/s), `settings: { strength, min_speed, ratio_min, ratio_max, response, mode: "Rate"\|"Pose" }` (default full strength, `Pose`), `legs: [[hip, knee, ankle], …]` and `pelvis` (needed in `Pose` mode), `stride_length` (meters, default 0.5); input `inner`. Matches the inner clips' speed to the ground speed by clock or stride IK (v4, ADR-8540) |
| `orientation_warp` | `heading_param` (float, radians, 0 = forward, counter-clockwise), `pelvis`, `spine: [[name, share], …]` (shares sum to 1), `strength` (default 1), `max_delta` (radians, default π/2); input `inner`. Turns the lower body toward the heading, the spine back (v4) |

## Build a graph

```json
{"method": "editor.anim_open", "params": {"create": true, "path": "anim/hero.animgraph.ron"}}
{"method": "editor.anim_param", "params": {"name": "Speed", "kind": "float"}}
{"method": "editor.anim_param", "params": {"name": "Grounded", "kind": "bool", "default": true}}
{"method": "editor.anim_add_node", "params": {"kind": "state_machine"}}
{"method": "editor.anim_state", "params": {"machine": 1, "op": "add", "name": "Idle"}}
{"method": "editor.anim_add_node", "params": {"kind": "clip", "scope": {"machine": 1, "state": "Idle"},
  "data": {"clip": "Survey", "sync_group": "gait"}}}
{"method": "editor.anim_connect", "params": {"child": 2}}
{"method": "editor.anim_transition", "params": {"machine": 1, "from": "Idle", "to": "Walk",
  "conditions": [{"Float": {"param": "Speed", "op": "Gt", "value": 0.5}}], "duration": 0.25}}
{"method": "editor.anim_connect", "params": {"child": 1}}
{"method": "editor.anim_save", "params": {}}
```

Conditions: `{"Float": {param, op, value}}`, `{"Int": …}`, `{"Bool": {param, value}}`,
`{"Trigger": {param}}`, `"ClipFinished"`, `{"Notify": "Land"}`; `op` is `Lt Le Gt Ge Eq Ne`.
`from: null` is "any state". `editor.anim_state` ops: `add remove rename initial move` (`rename` takes `new_name`).
`editor.anim_transition` takes `interruptible` (default true): whether a later transition may cut this one short.
`editor.anim_transition` ops: `add` (default), `set { index }`, `remove { index }`. `blend` is
`Crossfade` (default) | `Inertialize` | `InertializeSpring` (see
[Inertialized transitions](inertialization.md)). `duration` is the cross-fade length or the
inertial decay time.

Editing nodes after the fact: `editor.anim_set_node { node, data?, pos? }` shallow-merges `data`
into the node's kind fields (a type mismatch is an error, nothing changes) and/or moves it;
`editor.anim_remove_node { node }` deletes it (a state machine takes its states' sub-graphs
with it); `editor.anim_disconnect { parent, slot }` clears one input, and
`editor.anim_disconnect { scope }` without `parent` clears that canvas's output.

`editor.anim_graph { ron: true }` returns every node (`id, kind, scope, pos, inputs, data`; state
machines list `states`, `initial`, `transitions`), `valid` / `error` (the same validation the
runtime loader runs), `unreachable` (nodes no output reaches — they are not saved), `paths`
(node id → runtime path) and the RON `anim_save` would write. `anim_save` refuses an invalid
graph and reports the unreachable nodes it dropped.

The end-to-end test builds `samples/third_person`'s locomotion graph (three states, five
transitions, four foot IK wrappers) with these calls only; the result is committed as
`samples/third_person/assets/locomotion.animgraph.ron` and equals the sample's code-built graph.

## Preview and PIE debugging

`editor.anim_rig { source: "demo" | "selected" | "gltf", path? }` picks the preview character:
a built-in biped (clips `Idle Survey Walk Run Wave Jump`, the sample's names), the selected
entity's `SkinnedMesh` + `Animator` clips, or a `.glb` (its `.animedit.ron` opens with it).

`editor.anim_preview { params: { "Speed": 3.0, "Jump": true }, advance: 0.5, speed?: 1.0 }` sets preview
parameters, advances at 60 Hz and returns `{ state, error, nodes: [{ path, node, active, weight,
state }], parameters, speed }` — the evaluator is the game's, with the rig's edits applied. `speed` (0 to 2)
is the panel's preview speed slider: it scales every preview step, an agent's `advance` included, so at 0 the
preview does not move (ADR-7747).

While PIE runs, the Anim Graph panel mirrors the play world's animator running the open graph
(or the first graph animator): `editor.anim_debug` returns the same snapshot for it, plus
`entity` and `name`; `null` when nothing runs. Node `state.kind` is `clip` (time, finished),
`blend_space` (input, per-clip weights), `state_machine` (current, transition `[from, to,
progress]`), `blend` (alpha), `layers` (weights) or `wrapper`.

## Clips and skeletons

`editor.anim_clip { clip, op, … }` edits one clip of the preview rig: `add_notify { time, name,
payload? }`, `set_notify { index, … }`, `remove_notify { index }`, `root_motion { mode }`,
`set_curve { name, keys: [[time, value, "linear"|"constant"|"cubic", slope?], …], property? }`,
`remove_curve { name, property? }`. `property: true` targets a property track (`name` is the
reflected path, e.g. `Transform.translation.y`). The first edit of a clip copies its imported
notifies into the sidecar, so they are kept.

`editor.anim_skeleton { op, name, … }`: `add_socket { joint, offset: [x, y, z], rotation?: [yaw, pitch, roll] }`
(`rotation` in degrees, Euler YXZ, the panel's Rotation° fields; absent = no rotation; ADR-7747),
`remove_socket`, `add_ik_chain { joints: [root, mid, tip], pole_param? }` (must be a
parent → child chain), `remove_ik_chain`. Graph IK nodes pick authored chains in the inspector.
`editor.anim_save_edits { path? }` writes the sidecar.

## The standalone timeline and pose sampling

Two reusable pieces that do not need the Clip panel or an `AnimEditor` document:

- **`regia_editor::anim::timeline::Timeline`** is an egui widget (`timeline.show(ui)`) with
  tracks of keys, a playhead, a loop range, a Loop toggle and a marker lane (foot contacts,
  notifies). It owns its data and returns `TimelineEvent`s (`Scrub`, `KeyMoved`, `KeyAdded`,
  `KeyRemoved`, `MarkerMoved`, `MarkerAdded`, `MarkerRemoved`, `RangeChanged`, `LoopChanged`,
  `Selected`); drags emit `finished: true` once on release, which is the one undo step.
  Click or drag the ruler to scrub, shift-drag it to draw the range (drag its ends, secondary
  click clears it), drag keys and markers, double click a track or the marker lane to add one,
  secondary click or Delete removes, Ctrl + wheel zooms. `Timeline::from_clip` fills it from a
  clip.
- **`regia_animation::sample_into(world, skeleton_root, &clip, time, SampleMode)`** writes the
  clip's joint TRS at `time` into the joint entities of any world, with the animation path's own
  sampling and additive math. Only joints the clip animates are written, so a clip over the first
  67 of 109 joints leaves the other 42 alone. Time clamps to the clip; wrap it first to loop.

The agent drives the widget headless, with the same logic the egui frame uses:

`editor.timeline_set { clip?, duration?, playhead?, range?, loop?, snap?, tracks?, markers?,
width?, zoom? }` fills it (`clip` loads one track per animated joint and a marker per notify;
`Foot…` notifies are foot contacts). `editor.timeline_state` reads it and drains the edit events
since the last call. `editor.timeline_input { op: "press"|"drag"|"release"|"delete", time, row:
"toolbar"|"ruler"|"markers"|<track index>, button?, shift?, double? }` is synthetic pointer input
and returns the events it caused:

```json
{"method": "editor.timeline_set", "params": {"clip": "Walk", "snap": 0.0333}}
{"method": "editor.timeline_input", "params": {"op": "press", "row": "ruler", "time": 0.5}}
{"method": "editor.timeline_input", "params": {"op": "press", "row": 0, "time": 0.4}}
{"method": "editor.timeline_input", "params": {"op": "release", "row": 0, "time": 0.6}}
{"method": "editor.timeline_state", "params": {}}
```

`editor.sample_into { clip, time, mode?: "replace"|"additive", joints? }` samples a preview-rig
clip into a scratch copy of the rig and returns `{ written, untouched, joints: [{ name,
translation, rotation, scale }] }` for the animated joints (or the named ones).

## Seeing the panels

`editor.capture_panel { panel: "anim_graph" | "anim_clip" | "anim_skeleton", width?, height? }`
(default 1024 x 640, clamped to 64..=4096) renders a panel offscreen and returns `{ width, height,
mime: "image/png", png_base64 }`. The panel shows
whatever the editor state selects (canvas, selected node/state/transition, clip, curve, joint).

## Live parameters, the recorder and the clip preview

These are view and debug tools: none of them edits a saved document or adds an undo entry.

**Live parameters.** While play-in-editor runs and the Anim Graph panel watches an animator, the
Parameters section shows a **Live** badge and its fields write to that animator (a debug write: not
saved, forgotten when play stops). The agent writes the same way:

```json
{"method": "editor.anim_param", "params": {"name": "Speed", "live_value": 2.5}}
```

returns `{ parameters, live: { name, value, queued } }`; the write reaches the animator at the
panel's next frame. `live_value` takes a number, a bool, `[x, y]` / `[x, y, z]`, or anything for a
trigger, converted to the declared kind.

**The recorder.** The Recorder section above the parameters records the watched animator while
play runs: per play frame its current state, transition, parameter values, node blend weights and
evaluated pose (with a 64-bit pose hash). The ring keeps `capacity` frames (default 600, at most
36 000) and never more than 64 MiB; the oldest frames go first and are counted. A paused session
records nothing. After pause or stop, the frame slider (arrow keys step one frame) replays a
frame: the graph canvases highlight its states and nodes, `editor.anim_debug` returns it, and the
graph preview takes its pose. **Live** follows the session again.

- `editor.anim_record { op: "start", capacity? }`, `{ op: "stop" }`, `{ op: "state" }` return
  `{ recording, frames, capacity, bytes, dropped, scrub, first, last }`.
- `editor.anim_scrub { frame }` (0 = the oldest kept frame) returns `{ frame, index, play_frame,
  entity, name, state, transition: { from, to, progress }, parameters, weights: [{ path, weight }],
  pose_hash }`; `frame: null` goes back to live.

**The clip preview.** The Clip panel's playback row sets the speed (0.1 to 4x), **Once**, **Loop**
or **Ping-pong**, and a preview range (the two range fields, or drag the bright end line on the
timeline; the shaded part is not played; **Whole clip** clears it). The range trims only what the
preview plays, never the clip. **Onion skins** draws ghost poses before (z-axis color) and after
(ok color) the playhead: count, step in seconds and opacity. **Root-motion floor** scrolls a floor
grid under the in-place rig by the clip's extracted root motion and draws the root's path on it.
The clip picker has a search field and sorts by import order, name, length or recent use.

```json
{"method": "editor.anim_clip", "params": {"op": "preview", "clip": "Walk", "speed": 0.5,
  "loop": "ping_pong", "range": [0.2, 0.8], "onion": {"enabled": true, "before": 2, "after": 2,
  "step": 0.1, "opacity": 0.5}, "root_floor": true, "time": 0.2, "advance": 0.5}}
{"method": "editor.anim_clip", "params": {"op": "list", "filter": "run", "sort": "length"}}
```

`preview` returns `{ clip, time, playing, speed, loop, reverse, range, trimmed, onion,
onion_times: [[offset, time], …], root_floor, root: { position, yaw } }` (`root` is the root's
travel from the range start to `time`); `list` returns `{ clips: [{ name, duration }], filter,
sort }`.

**Several curves at once.** The Clip panel's curve list and the Sequence Curves panel's curve list
show every curve in a palette color (theme tokens), each with a show checkbox and a lock checkbox;
**Frame all** fits the view to every visible curve. Hidden curves are not drawn; the other visible
curves are drawn behind the edited one; a locked curve still scrubs but refuses edits. The agent
reads and sets the same state:

```json
{"method": "editor.curve_view", "params": {"panel": "seq_curve", "curve": "track:1:2", "locked": true}}
```

returns `{ panel, curves: [{ key, label, palette_index, visible, locked, edited }] }`. Keys are
`curve:<name>` and `property:<path>` (Clip panel) and `track:<index>:<component>` (Sequence
Curves).

**Keyboard.** Every control above is a focusable widget: Tab reaches it, arrow keys change sliders
and number fields, Space or Enter toggles checkboxes and presses buttons, and the clip picker opens
with Enter and takes typed search text.

**Error codes** (in `error.data.code`):

| Code | When |
|---|---|
| `not_playing` | `live_value` without a watched play animator, or while a recording is replayed |
| `unknown_parameter` | `live_value` for a parameter the graph does not declare |
| `nothing_recorded` | `editor.anim_scrub` before anything was recorded |
| `frame_out_of_range` | `editor.anim_scrub` past the last recorded frame |
| `bad_range` | `editor.anim_clip { op: "preview", range }` that is not `0 <= start < end` |
| `unknown_curve` | `editor.curve_view` with a key the panel does not list |

## Chain IK and body aim nodes

`chain_ik` and `body_aim` (`.animgraph.ron` v4, ADR-8475) are node kinds of `editor.anim_add_node` and
`editor.anim_set_node` like the rest; there is no new method. A joint limit in `data` is `"Free"`,
`{ "Cone": { "max_swing", "min_twist", "max_twist" } }` or `{ "Hinge": { "axis": [x, y, z], "min", "max" } }`
(radians, relative to the joint's bind rotation). A solver or limit name the format does not know fails the
call with nothing changed. A `body_aim` joint with an empty name is refused when the graph is saved (validation,
ADR-8732), and the inspector's "Add joint" appends a rig joint, never an empty name. In a `.animgraph.ron` file the
two nodes and a `PerJoint` entry refuse unknown fields (a misspelled field is a load error). Every add, edit and connection is one undo step.

```json
{"method": "editor.anim_add_node", "params": {"kind": "chain_ik", "data": {
  "root": "tail_01", "tip": "tail_06", "solver": "Ccd", "target": {"Param": "TailTarget"},
  "limits": {"Uniform": {"Cone": {"max_swing": 0.6, "min_twist": -0.2, "max_twist": 0.2}}}}}}
{"method": "editor.anim_add_node", "params": {"kind": "body_aim", "data": {
  "joints": [["turret_yaw", 1.0], ["turret_pitch", 1.0]], "aim_joint": "muzzle", "target": {"Param": "Aim"}}}}
```

At run time the nodes log, once per rig and name: `animgraph_chain_invalid` (the root and tip do not form a
parent chain; the node passes the pose through), `animgraph_limit_joint_not_in_chain` (a per-joint limit names
a joint the node does not turn; ignored) and `animgraph_aim_joint_not_under_chain` (the aim joint is neither a
turning joint nor below the last one). An unknown joint name stops the graph's evaluation with the same
"joint not found" error `two_bone_ik` gives. Residuals (the last chain node's tip distance and iterations, the last aim node's angle) are kept per rig in
`EvalReuse::ik_node_residuals` for `anim.ik_report`.

## Stride and orientation warp nodes

`stride_warp` and `orientation_warp` (`.animgraph.ron` v4, ADR-8540) are node kinds of `editor.anim_add_node` and
`editor.anim_set_node`; there is no new method. A `Pose`-mode stride warp without legs or a pelvis, settings out of
range (strength outside `[0, 1]`, `ratio_min` above 1, `ratio_max` below 1) or spine shares that do not sum to 1 fail
graph validation when the graph compiles, with nothing changed. Every add, edit and connection is one undo step.

```json
{"method": "editor.anim_add_node", "params": {"kind": "stride_warp", "data": {
  "ground_speed_param": "GroundSpeed",
  "settings": {"strength": 1.0, "min_speed": 0.1, "ratio_min": 0.8, "ratio_max": 1.4, "response": 12.0, "mode": "Pose"},
  "legs": [["thigh_l", "calf_l", "foot_l"], ["thigh_r", "calf_r", "foot_r"]], "pelvis": "pelvis"}}}
{"method": "editor.anim_add_node", "params": {"kind": "orientation_warp", "data": {
  "heading_param": "MoveHeading", "pelvis": "pelvis", "spine": [["spine_01", 0.5], ["spine_02", 0.5]], "max_delta": 1.0}}}
```

At run time an inner graph without root motion makes the node pass the pose and log `animgraph_warp_no_root_motion`
once per node and parameter; an unknown joint name stops the evaluation with the "joint not found" error
`two_bone_ik` gives. What each node did on the latest frame is in `anim.get_state`'s `warps` (see
[Animation](animation.md#locomotion-warp-nodes)) and in the editor preview's debug snapshot.

## Locomotion test bed

`editor.anim_locomotion_preview` walks a gait in the animation preview's locomotion test bed (ADR-8806): the
stepper of `regia_animation::gait` in driven mode, at fixed 1/60 s steps, over a ground preset, with a driven body
that starts at the origin heading −Z and moves at `speed` while turning at `turn_rate`. It answers the run's quality
metrics, puts the run in the panel's test bed (Anim Graph tab, right column, "Locomotion test bed"), and with
`screenshot` renders that panel offscreen (locomotion gizmos on) and writes it as a PNG. The run is not an edit:
nothing goes on the undo stack. Results are deterministic for identical parameters on one machine.

| Parameter | Type | Meaning |
|---|---|---|
| `entity` | number | Entity id whose `GaitRig.params` are walked (hip offsets measured from the bind pose when the rig's `auto_hips` is on and it has a skeleton). Not with `asset`. |
| `asset` | string | Project-relative `.gait.ron`, read through the format's loader. Not with `entity`. Neither: the built-in six-leg tripod (the settings `anim.gait_simulate` uses without `params`). |
| `ground` | string or object | A preset name, or `{kind, ...}` overriding the preset's fields: `flat`; `slope {degrees}` (0 to 60, default 10); `stairs {rise, run}` (rise 0 to 1 m, default 0.15; run 0.05 to 10 m, default 0.3); `rubble {amplitude, cell, seed}` (0 to 2 m, default 0.1; 0.05 to 10 m, default 0.4; seed default 7); `tunnel {depth}` (the floor at 0 and a second layer `depth` meters above it, 0.5 to 50, default 3). Default flat. Slopes and stairs climb along −Z. |
| `speed` | number | Driver ground speed, 0 to 20 m/s (default 1). |
| `turn_rate` | number | Driver turn rate, −6.283 to 6.283 rad/s, positive turns left (default 0). |
| `seconds` | number | Simulated time, above 0 and at most 300 (default 5); `steps` is `seconds × 60` rounded. |
| `screenshot` | string | Project-relative `.png` path to write the test bed panel to (640 × 480). |

The result is `{ source, ground, speed, turn_rate, seconds, steps, dt, legs, metrics, final: { position, yaw },
screenshot }`. `source` is `"default"`, `{entity}` or `{asset}`; `screenshot` is `{path, width, height, bytes}` or
null. `metrics` uses the definitions of [`anim.gait_simulate`](locomotion-quality.md), and a straight run on flat,
slope, stairs and rubble gives that method's numbers bit for bit (`slope {degrees}` is its `slope {rise_z:
-tan(degrees)}`, `stairs {rise, run}` its `steps {rise: -rise, run, axis: "z"}`, the driver its `body_path
[[0, 0], [0, -4096]]`):

| Metric | Meaning |
|---|---|
| `max_slide` | Largest XZ move of a foot planted and not swinging before and after one step, meters (`max_planted_slip` there). |
| `max_foot_jump` | Largest 3D move of any foot in one step, meters. |
| `forced_steps` | Feet the reach limit forced to step. |
| `refused` | Footsteps refused (slope too steep, no ground). |
| `replants` | Feet moved to a better foothold. |
| `min_planted` | Fewest planted feet after any step. |
| `stability_margin` | The last step's margin of the body over the support polygon, meters; null when there is no polygon. |
| `min_stability_margin` | The smallest finite margin seen; null when none was finite. |
| `settled` | The walker is idle with every foot at neutral. |

```json
{"method": "editor.anim_locomotion_preview", "params": {"entity": 4294967321,
  "ground": {"kind": "stairs", "rise": 0.12, "run": 0.35}, "speed": 0.8, "turn_rate": 0.2, "seconds": 6,
  "screenshot": "captures/stairs.png"}}
```

| Error code | When |
|---|---|
| `F0002` | `entity` and `asset` together; `seconds` not a number above 0; a parameter of the wrong type. |
| `no_such_entity` | `entity` names no entity. |
| `no_gait_rig` | The entity has no `GaitRig`. |
| `gait_settings`, `gait_pattern` | The rig's settings fail the stepper's checks. |
| `gait_io`, `gait_parse`, `gait_version`, `gait_legs` | The `.gait.ron` cannot be read, is not a gait document, has a newer `format_version`, or its legs do not match its patterns (an asset's settings can also answer `gait_settings` or `gait_pattern`). |
| `F0002` with `reason: path_outside_project` | `asset` or `screenshot` leads outside the project (`data.path_reason` names the rule). |
| `bad_ground` | An unknown `ground.kind`, a field the kind does not have, or a value out of range. |
| `bad_driver` | `speed` or `turn_rate` not a finite number in range. |
| `simulate_too_long` | `seconds` above 300. |
| `gait_input`, `gait_state`, `gait_floor`, `gait_step_too_large` | The stepper stopped the walk mid-run (the message names the step). |
| `editor_headless` | `screenshot` on an editor without a GPU adapter. |
| `capture_failed` | Rendering, encoding or writing the screenshot failed. |

`editor.capture_panel {panel: "anim_locomotion"}` captures the panel as it stands. In the Inspector, a `GaitRig` or
`FootPlacement` shows an authoring section (settings with their ranges, leg chain pickers, pattern presets, the
`.gait.ron` picker). The field, joint and preset edits are ordinary component edits, so `editor.undo` reverts them and
agents make them with `editor.set_property` and inspect them with `scene.get_entity`. The picker's buttons have their
own methods:

| Method | What it does |
|---|---|
| `editor.anim_gait_load { entity, path }` | Load a project `.gait.ron` into one entity's `GaitRig` (params, legs and body) as one undoable component edit. Returns `{ entity, path, loaded, legs, body, auto_hips }`. |
| `editor.anim_gait_save { entity, path }` | Write one entity's `GaitRig` to a project `.gait.ron` file with the format writer's own validation and atomic write. Returns `{ entity, path, written, legs, body, auto_hips }`. |

Both methods refuse a dead `entity` as `no_such_entity`, an entity without a `GaitRig` as `no_gait_rig`, a path outside
the project with `path_outside_project`, and a bad gait file or settings with the same `gait_*` codes
`anim.gait_validate` / `anim.gait_simulate` use.
