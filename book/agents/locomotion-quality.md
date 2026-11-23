# Locomotion quality (agents)

Four methods (ADR-8708) let an agent judge procedural locomotion without a renderer:

| Method | What it answers | Mutating |
|---|---|---|
| `anim.locomotion_state` | One rig's gait stepper, foot-plant report and foot-placement status in one object | no |
| `anim.gait_simulate` | A headless run of the gait stepper over a synthetic ground: foot jumps, planted counts, reach, a pose hash | no |
| `anim.foot_plant_simulate` | The foot planter on the built-in demo humanoid's walk over a synthetic ground: slide, penetration, float, tilt | no |
| `anim.ik_report` | Per IK graph node: residual, iterations, clamp, weight (while the rig's IK debug flag is on) | yes (the flag) |

The two simulate methods are stateless: every call carries its settings, ground and inputs, and
the same call answers the same numbers on one target. They are presentation tools, never a
simulation checksum. The guide topic is `locomotion-quality` (`agent.guide`).

## `anim.locomotion_state`

`{ "entity": <id> }`. The entity may carry any of `GaitState` (the procedural stepper,
[Procedural gait](procedural-gait.md)), `FootPlantReport` and `FootPlacementStatus`
([Foot planting](foot-planting.md)); each part is `null` when its component is absent.

<!-- not-a-request -->
```json
{ "entity": 4294967310,
  "gait": { "mode": "driven", "legs": 6,
            "pattern": { "index": 0, "legs": 6, "duty_factor": 0.5, "phase_offset": [0, 0.5, 0.5, 0, 0, 0.5] },
            "blending": false, "blend": 0, "beat": 1, "phase": 0.42,
            "position": [0, 0.7, -3.1], "yaw": 0, "velocity": [0, 0, -1.2],
            "feet": [{ "position": [-0.4, 0, -2.6], "planted": true, "normal": [0, 1, 0],
                       "mode": "pattern", "phase": 0.21, "swinging": false }, "..."],
            "planted_count": 3, "refused": 0, "forced_steps": 0, "replants": 0, "settled": false,
            "power_lost": false, "stability_margin": -0.05,
            "attitude": { "pitch": 0.12, "roll": 0, "quat": [0.06, 0, 0, 0.998],
                          "body_height_target": 1.41 } },
  "foot_plant": { "weight": 1, "pelvis_offset": -0.02, "planted": [1, 0], "lock": [1, 0],
                  "tilt_deg": [3.1, 0], "slide": [0.003, 0], "max_slide": 0.003,
                  "heel_gap": [0, 0], "toe_gap": [0, 0] },
  "foot_placement": { "probe_installed": true, "legs_resolved": true, "lod": "full" } }
```

A foot's `mode` is `pattern` (follows its gait pattern), `forced` (an out-of-phase step forced by
reach), `waiting` (landed a forced step, waiting for its pattern turn) or `hanging` (no ground).
`stability_margin` is `null` when no foot is planted. `attitude` is the body posture of ADR-8706
([Procedural gait](procedural-gait.md), "Posture: body height and tilt"): `pitch` and `roll` in
radians (positive pitch raises the nose), `quat` the same tilt as `[x, y, z, w]` in the stepper's
local frame, and `body_height_target` the height the body eases toward. A
walker with `GaitPosture::UPRIGHT` reports exactly `0`, `0` and `[0, 0, 0, 1]`.
`foot_placement.lod` is the IK level of detail this frame (`full`, `reduced` or `off`, ADR-8728;
[Foot planting](foot-planting.md), "IK level of detail").

| Error | When | Fix |
|---|---|---|
| `no_such_entity` | the id is not a live entity | find rigs with `scene.query_entities` or `anim.list` |
| `no_locomotion` | the entity has none of the three components | spawn a `GaitState`, or give the rig a `FootPlacement` |
| `gait_state` | the entity's `GaitState` or `FootPlantReport` holds a non-finite number (a scene file or a reflected write left it; the message names the field). It is refused, never published as `null` (ADR-10053) | step the gait once (a driven or integrating step heals non-finite posture fields) or rebuild the state |

## `anim.gait_simulate`

Runs `regia_animation::gait` for `steps` fixed steps of `dt` and measures every step.

| Parameter | Default | Meaning |
|---|---|---|
| `params` | the six-leg default below | `GaitParams` fields written over the default |
| `ground` | flat at 0 | a synthetic ground, see "Grounds" |
| `inputs` | `[[1, 0, 0]]` | integrating mode: `[forward, strafe, steer]` in −1..1 per step, or `[forward, strafe, steer, steps]` held that many steps; past the end the last row is held |
| `body_path` | none | driven mode instead of `inputs`: `{ "points": [[x, z], ...], "speed": m/s }` (2 to 4096 points, speed 0..50); the body follows the polyline at that speed facing along it (−Z forward), then stops |
| `steps` | 600 | 1 to 20000; more is `simulate_too_long` |
| `dt` | 1/60 | seconds, above 0 and at most 0.1 |
| `trace_every` | 0 (none) | a trace row every N steps; at most 2000 rows |
| `gait_asset` | none | instead of `params`: a project-relative `.gait.ron` ([Procedural gait](procedural-gait.md), "Gait files"), read by the loader a `GaitRig` uses, so the run steps exactly that file's settings, posture included; not together with `params` (`gait_settings`) |

The default walker is the six-leg tripod of the agent guide: `forward_speed` 2.4, `reverse_speed`
1.1, `strafe_speed` 0.8, `acceleration` 3, `step_length` 0.6, `clearance` 0.7, `swing_lift` 0.18,
`max_slope` 0.9, `idle_speed` 0.02, `max_yaw_rate` 0.8, hips `[±0.4, 0.5]`, `[±0.5, 0]`,
`[±0.4, −0.5]` (`[right, forward]`, left first), `settle_distance` 0.05, `settle_period` 0.3,
`turn_step_distance` 0.15, `turn_step_angle` 0.35, `ground_band` 2, `foothold_candidates` 16,
`foothold_spacing` 0.25, `max_reach` 1.2, `hang_drop` 0.3. Besides the scalar fields, `params`
takes `hip_offsets` (1 to 16 `[right, forward]` pairs; their count is the leg count),
`max_reach` (one number, or one per leg), `foothold_candidates` (0..64), `patterns` (a preset
name or a list of up to 4: `tripod`, `ripple`, `wave`, `quadruped_walk`, `trot`, `pace`, `bound`,
`biped_walk`; `ripple` and `wave` take the leg count from `hip_offsets`), `switch_speeds` (one
fewer than the patterns), `hysteresis` and `posture` (ADR-10130): an object of the `.gait.ron`
posture fields `swing_weight`, `height_tau`, `height_slew` (`null` is no limit), `attitude_weight`,
`max_pitch`, `max_roll` and `attitude_tau`, each written over `GaitPosture::UPRIGHT` (unset fields
stay upright: no smoothing, no tilt). A non-object, an unknown field or a value out of its range is
`gait_settings`. `{ "params": { "posture": { "attitude_weight": 1, "max_pitch": 0.6, "max_roll": 0.6 } } }`
pitches the body on a slope; read it in `final.attitude`.

```json
{ "steps": 900,
  "ground": { "kind": "rubble", "amplitude": 0.12, "cell": 0.35, "seed": 7 },
  "params": { "hip_offsets": [[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]],
              "patterns": ["quadruped_walk", "trot"], "switch_speeds": [1.0], "hysteresis": 0.1 },
  "inputs": [[1, 0, 0, 300], [0, 0, 0, 120], [0, 0, 1, 180]],
  "trace_every": 100 }
```

The answer is `{ mode, steps, dt, legs, metrics, final, trace }`; `final` has the shape of
`anim.locomotion_state`'s `gait`, `trace` is `null` or rows `{ step, position, yaw, phase,
pattern_index, planted, feet }`.

| Metric | Meaning | A healthy walk |
|---|---|---|
| `max_foot_jump` | largest 3D distance any foot moved in one step, meters | at most `swing_bound` |
| `swing_bound` | the most a swing arc can move a foot in one step (formula below) | |
| `max_planted_slip` | largest horizontal move, in one step, of a foot planted before and after it | 0 (a pop or slide shows here) |
| `min_planted` | fewest feet planted after any step | the pattern's stance count |
| `max_reach_ratio` | largest planted foot-to-hip distance over that leg's `max_reach`; `null` with no finite reach, 0 when no foot with a finite reach was planted after a step (ADR-10053) | below 1 |
| `max_hip_distance` | the same distance in meters | |
| `max_speed` | largest body ground speed seen, m/s | |
| `refused`, `forced_steps`, `replants`, `settled` | the stepper's counters and rest flag at the end | 0, 0, 0, true after a stop |
| `stability_margin`, `min_stability_margin` | the final and the smallest static stability margin (`null`: never a planted foot) | positive for statically stable gaits |
| `final_pose_hash` | 16 hex digits over the final body, feet and counters | equal for equal calls |

`swing_bound = dt * ((L + 4 * swing_lift) / T + v + max_yaw_rate * R)`, where `L = step_length /
d + 2 * step_length + ceil(foothold_candidates / 8) * foothold_spacing` is the longest swing (`d`
is the smallest duty factor of the patterns), `T = min(settle_period, (1 - d) / d * step_length /
v)` the shortest swing time, `v` the largest body speed seen and `R` the largest hip radius plus
`step_length`. A foot that teleports (a replant after a driven body jump, an idle snap to neutral)
moves a planted foot and shows in `max_planted_slip`.

Stop-and-turn check (the idle stepping of ADR-8539):

```json
{ "steps": 420, "inputs": [[0.5, 0, 0, 120], [0, 0, 0, 60], [0, 0, 1, 120], [0, 0, 0, 120]] }
```

then assert `metrics.max_foot_jump <= metrics.swing_bound`, `metrics.max_planted_slip == 0`,
`metrics.replants == 0` and `metrics.settled == true`.

| Error | When | Fix |
|---|---|---|
| `gait_settings` | a `params` field is unknown, not a finite number or outside its range; or finite settings drove a metric past `f32` (a huge `step_length` overflows `swing_bound`, a tiny `max_reach` overflows `max_reach_ratio`; the message names the metric, ADR-10053) | fix the named field |
| `gait_input` | `inputs`, `body_path` or `dt` is malformed, or both `inputs` and `body_path` are given; a missing, `null` or wrong-typed `body_path.points` or `body_path.speed` also names the field in `data.param` and its type in `data.expected` | send rows of 3 or 4 numbers, or one `body_path`; 0 < `dt` <= 0.1 |
| `gait_pattern` | an unknown pattern, mixed leg counts, bad `switch_speeds`, or `hip_offsets` not one per leg | use the preset names; one hip per leg |
| `gait_step_too_large` | a step would advance more than one stance; `data.step` names it | smaller `dt`, longer `step_length` or a lower speed |
| `gait_state` | the stepper's state overflowed; the documented ranges do not reach it | report it with the call |
| `gait_floor` | a ground sample was not finite; the documented ground ranges do not reach it | keep ground fields in range |
| `simulate_too_long` | `steps` above 20000, or more than 2000 trace rows | split the run; raise `trace_every` |
| `bad_ground` | see "Grounds" | fix the ground object |
| `gait_parse`, `gait_version`, `gait_legs`, `gait_io` | the `gait_asset` file does not parse, has another format version, a bad leg list, or cannot be read (its settings answer `gait_settings` and `gait_pattern` as above) | check it with `anim.gait_validate` |

A missing or mistyped `steps` or `trace_every` is `F0002`; a `gait_asset` that does not exist is
`F0003`.

Simulate a gait file (the roll-up's ADR-8737):

```json
{ "steps": 600, "ground": { "kind": "slope", "rise_z": -0.176 },
  "gait_asset": "assets/units/walker.gait.ron" }
```

then read `final.attitude.pitch` to see the posture the file asks for.

## Grounds

Both simulate methods take the same `ground` object; heights are meters, +Y up.

| `kind` | Fields (default) | Surface |
|---|---|---|
| `flat` | `height` (0) | level |
| `slope` | `height` (0), `rise_x` (0), `rise_z` (0), each in −5..5 | `y = height + rise_x * x + rise_z * z`, its normal |
| `steps` | `height` (0), `rise` (0.15, −2..2), `run` (0.3, 0.05..100), `axis` (`"z"` or `"x"`) | treads `run` deep, each `rise` higher along +axis |
| `rubble` | `height` (0), `amplitude` (0.1, 0..2), `cell` (0.4, 0.05..100), `seed` (0) | square blocks `cell` wide at a seeded height in `[0, amplitude)` |

`bad_ground`: `ground` is not an object, `kind` is not one of the four names (a `kind` that is not a
string, such as `7`, too; only an absent or `null` `kind` is flat), a field the kind does not
have, or a field that is not finite or outside its range. The same seed gives the same blocks.

## `anim.foot_plant_simulate`

Plays the demo humanoid's walk (`regia_animation::demo_rig`) in place while the rig moves
`speed_scale` times the walk's authored 1.3 m/s along −Z (turned by `yaw`), plans every frame
with `FootPlanter` and poses it with `apply_foot_plant`, as the foot-planting acceptance walk
does. Measurements start after half a second.

| Parameter | Default | Meaning |
|---|---|---|
| `settings` | `FootPlantSettings::default()` | fields written over the defaults (`lock`, `max_tilt`, ...); an unknown field, or a number past the `f32` range such as `1e40` (it would be stored as infinity), is `F0002` |
| `ground` | flat at 0 | see "Grounds" |
| `speed_scale` | 1.15 | 0..4; 1.15 makes the clip skate about 10 cm per stance without the lock |
| `yaw` | 0 | walk heading, radians |
| `steps` | 180 | 1 to 20000 frames; more is `simulate_too_long` |
| `dt` | 1/60 | frame length, above 0 and at most 0.1 |

| Metric | Meaning |
|---|---|
| `stances` | planted stances measured |
| `max_slide` | largest horizontal drift of a solved planted ankle from its touchdown, meters (at most 0.01 with the lock on flat ground) |
| `target_slide` | the same for the planner's world target |
| `max_penetration`, `max_float` | how far a planted ankle sat below or above the ground plus the tilted ankle height, meters |
| `max_tilt_deg`, `mean_tilt_deg` | foot tilt toward the ground normal over planted frames |
| `largest_step` | largest per-frame move of a solved ankle (release pops show here) |
| `pelvis_min` | lowest pelvis offset the planner chose (zero or negative) |

Errors: `simulate_too_long`, `bad_ground` (as above).

## `anim.ik_report`

`{ "entity": <id>, "enable": true | false }`. The rig's animation graph records, for each
`TwoBoneIK`, `LookAt`, `ChainIK` and `BodyAim` node, what its solve reached, but only while the
rig's IK debug flag is on (`Animator::set_ik_debug` in Rust). With the flag off the evaluator
measures nothing: `captures` stays 0. `enable` sets the flag before the answer, so a first call
with `enable: true` answers no rows yet; step a frame (`play.step`) and call again.

<!-- not-a-request -->
```json
{ "entity": 4294967310, "debug": true, "captures": 30, "graph": true,
  "nodes": [
    { "path": "root", "kind": "two_bone_ik", "solved": true, "residual": 0.00002,
      "iterations": 1, "clamped": false, "weight": 1, "graph_weight": 1 },
    { "path": "root/inner", "kind": "look_at", "solved": true, "residual": 0.0001,
      "iterations": 1, "clamped": false, "weight": 1, "graph_weight": 1 },
    { "path": "root/inner/inner", "kind": "chain_ik", "solved": true, "residual": 0.00001,
      "iterations": 4, "clamped": false, "weight": 1, "graph_weight": 1 } ] }
```

`path` is the node path of `anim.get_state`'s graph snapshot. `residual` is meters for
`two_bone_ik` and `chain_ik` and radians for `look_at` and `body_aim`; `two_bone_ik` and `look_at`
measure the final (weighted) pose, `chain_ik` the unblended solve. `clamped` means the target was
out of reach: outside the two-bone annulus, past the chain's length from its root, on a look-at
yaw or pitch limit (exact at weight 1), or more than 1 mrad left by a body aim's limits.
`solved: false` means the node did not solve on that frame (weight 0, unresolved target, invalid
chain, or an inactive branch). `captures` counts measurements since the rig was created.

| Error | When | Fix |
|---|---|---|
| `no_such_entity` | the id is not a live entity | `anim.list` |
| `no_animator` | the entity has no `Animator` | `anim.list` |

## Owed

- The rig-driver methods arrive with their lane (animation-06).

## Worked scene

`samples/anim_lab/tests/locomotion_agent.rs` (`locomotion_agent_dogfood`, ADR-8807) is a complete
conversation: it pauses the anim_lab locomotion scene, finds the walkers with `scene.query_entities`
(`ScriptedWalker`, `GaitRig`, `FootPlacement`), steps it with `play.step`, asserts on `GaitState`,
`GaitRigStatus`, `FootPlantReport` and `ScriptedWalker.frame` paths with `play.expect`, and
reads `anim.locomotion_state` for the hexapod (on the steps and on the slope) and the biped (on the
stairs). See [Procedural gait](procedural-gait.md#worked-scene-the-anim_lab-locomotion-lab).
