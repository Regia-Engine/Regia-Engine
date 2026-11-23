# Animation: animator state and graph parameters

See what every animated rig is doing and drive its anim graph while the game runs. The calls
need `regia_animation::AnimationPlugin` and a rig with an `Animator`; entity ids come from
`anim.list` (or `scene.query_entities`). A rig without an animator returns `NOT_FOUND` with a
hint.

## List rigs

`anim.list` returns every animator with the same summary `anim.get_state` gives for one:

```json
{}
→ { "count": 2, "animators": [ { "entity": 4294967310, "source": "graph", "state": "Loop", ... } ] }
```

## Read one animator

`anim.get_state` reports the current state-machine state (`null` when the graph root is not a
state machine), the parameters object below, the graph's declarations
(name, type, default), the root motion of the last evaluation plus what is pending for the
character controller, and the ragdoll mode and shown physics weight. Each kind lives in its own
map. The object has one key per name: when the same name was stored as more than one kind, the
later kind in the order float, int, bool, vec2, vec3, trigger replaces the earlier one.
`Animator::set` writes the Rust value's kind and does not clear the other maps, so a float
condition still reads the float while this object can show the later kind. An explicit type that
contradicts the graph's declaration is refused; that call is below.

```json
{ "entity": 4294967310 }
→ { "entity": 4294967310, "source": "graph", "state": "Loop", "transition": null,
    "clip": null, "speed": 1.0,
    "parameters": { "Speed": { "type": "float", "value": 1.6 },
                    "Land": { "type": "trigger", "value": false } },
    "declared": [ { "name": "Speed", "type": "float", "default": 0.0 } ],
    "root_motion": { "delta": [0, 0, -0.024], "yaw": 0.0, "pending": [0, 0, -0.024],
                     "pending_seconds": 0.016 },
    "ragdoll": { "mode": "powered", "blend": 0.0, "weight": 0.0, "bodies": 11, "reacting": 0,
                 "recovering": false } }
```

`transition` is `null` unless the first state machine is mid-transition, when it is
`{ machine, from, to, progress, blend, inertial_offset }`. `blend` is `crossfade`, `inertialize`
or `inertialize_spring`. See [Inertialized transitions](inertialization.md).
`clip` is `{ name, duration, looping, morph_tracks }` for single-clip animators. `root_motion` is `null`
until the first evaluation; `ragdoll` is `null` without a `Ragdoll` component.

## Set a parameter

`anim.set_param` writes one parameter; the graph reads it on the next frame. The type is
`type` when given, else the graph's declaration, else inferred from `value` (number → float,
boolean → bool, `[x, y]` → vec2, `[x, y, z]` → vec3). Fire a trigger with
`"type": "trigger"` (the value is ignored); it resets when a transition consumes it. Passing a
`type` that contradicts the declaration is `INVALID_PARAMS`.

```json
{ "entity": 4294967310, "name": "Speed", "value": 6.0 }
→ { "entity": 4294967310, "name": "Speed", "type": "float", "before": 1.6, "after": 6.0 }
{ "entity": 4294967310, "name": "Land", "type": "trigger" }
→ { ..., "type": "trigger", "before": false, "after": true }
```

It needs the `play` permission (it drives the running game).

## Verifying a transition

Set the parameter, wait for the state, then look:

1. `anim.set_param { entity, name: "Speed", value: 6.0 }`
2. Step or let the game run (`play.step`), then poll `anim.get_state` until `state` is
   `"Run"` (`play.wait_until` reads reflected components; `Animator` is not reflected).
3. `seeing.screenshot` to check the pose by eye.

## Ragdolls

`anim.get_state`'s `ragdoll` (and `anim.list`'s) reports the mode, the authored `blend`, the
shown physics `weight`, the built `bodies`, how many bones are `reacting` to a hit, and whether
a get-up is `recovering` (the fallen pose still easing out). Two methods drive it (both need the `play` permission):

- `anim.ragdoll_set { entity, mode?, blend?, blend_time?, strength?, tracking_time?,
  recover_snap? }` switches the mode (`kinematic` hit boxes, `powered` hit reactions, `limp`
  death, `active` motor-driven balance) and tunes it. `blend` and `strength` are 0..=1, `blend_time` >= 0, `tracking_time` > 0.
  Every field is checked before any is applied, so a refused call changes nothing; a call with
  no field is refused. Going limp keeps the animated momentum (a run carries into the fall).
  Switching from `limp` back to `kinematic` or `powered` is the get-up: the bodies re-pin to the
  animation, the rig moves (x/z and yaw) to where the root body lies when `recover_snap` is true
  (the default), and the fallen pose blends out over `blend_time`.
- `anim.ragdoll_hit { entity, joint, impulse, reaction?, recover? }` applies `impulse`
  (`[x, y, z]`, kg·m/s) to the bone nearest `joint` and raises that bone's and its children's
  physics weight to `reaction` (0..=1, default 1), decaying over `recover` seconds (> 0,
  default 1). A joint the rig's skeleton does not have is refused.

```json
{ "method": "anim.ragdoll_set", "params": { "entity": 4294967310, "mode": "limp" } }
→ { "entity": 4294967310, "mode": "limp", "blend": 0.0, "weight": 0.0, "bodies": 11,
    "reacting": 0, "recovering": false }
```

Both answer `F0003` for a missing entity or one without a `Ragdoll`, and `F0002` for a bad
value. Hits from gameplay go through `regia_framework::ragdoll::RagdollHit`, the same message
`anim.ragdoll_hit` writes. Bones with `RagdollLimits` get swing-twist joint limits centred on the
bind pose; the summary also reports `settled`, `face_up` and `frozen`, and `anim.ragdoll_settings`
sets the limits switch and the simulated-ragdoll budget (refusing a non-boolean `joint_limits` with
`ragdoll_bad_flag` and a bad `max_simulated` with `ragdoll_bad_budget`; see [character physics](character-physics.md),
ADR-1693).

## Contact curves in `anim.clip_report` (ADR-8308)

Each clip of `anim.clip_report` lists its cooked foot-contact curves under `contact_curves`, one entry
per curve named `contact.<foot joint>`: `name`, `joint` (the foot joint), `keys` (stored keys),
`planted_share` (share of the clip's frames where the curve is at or above 0.5) and `touchdowns`
(rising edges inside the clip). An empty list means the clip has none (the source's `.meta` did not set
`contact_curves`, or no feet were found; the cook warns). No new error codes: the method's existing
errors apply. A walk cooked with contact curves shows two entries with `planted_share` near the gait's
duty factor and about one touchdown per foot per cycle:

```json
anim.clip_report { "entity": 4294967310, "clip": "walk", "compression": false }
```

The curves feed the foot planter (see [foot planting](foot-planting.md)).

## Locomotion warp nodes

`anim.get_state` (and each `anim.list` row) has a `warps` list: one object per `StrideWarp` or `OrientationWarp`
graph node (`.animgraph.ron` v4, ADR-8540), depth first, with the node's debug path and what it did on the latest
frame. Before the first evaluation the nodes are reported at rest (ratio 1, turn 0); a single-clip animator or a
graph without warp nodes gives `[]`.

```json
"warps": [
  { "path": "root", "kind": "orientation_warp", "active": true, "delta": 0.35, "clip_heading": 0.0 },
  { "path": "root/inner", "kind": "stride_warp", "active": true, "mode": "pose", "ratio": 1.3,
    "clip_speed": 1.3, "ground_speed": 1.69, "pelvis_drop": 0.012 }
]
```

`ratio` is the smoothed ground-speed over clip-speed ratio (the clock multiplier in `rate` mode, the stride scale in
`pose` mode); `clip_speed` and `ground_speed` are m/s; `pelvis_drop` is how far `pose` mode lowered the hips
(meters); `delta` is the lower-body turn and `clip_heading` the inner root-motion heading (radians, 0 = forward,
counter-clockwise). To check a warp: set the ground-speed or heading parameter with `anim.set_param`, step with
`play.step`, then poll `anim.get_state` until `ratio` or `delta` settles.

| warning (log) | when |
|---|---|
| `animgraph_warp_no_root_motion` | a warp node's inner graph has no root motion while the warp should act (the node passes the pose); once per node and parameter |
