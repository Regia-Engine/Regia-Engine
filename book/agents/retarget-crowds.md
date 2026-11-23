# Retargeting on real content and crowds: check, observe

Spec: `docs/specs/85-retarget-crowds.md` (ADR-1857).

- `anim.retarget_check` checks how well one rig's animation retargets onto another rig before
  anyone looks at it on screen.
- `anim.crowd_status` reads a crowd's counters and one agent's playback state.

Both methods are read-only.

## Check a retarget

Two rig entities with a `SkinnedMesh` are needed. `clip` names a clip that the source rig's
animator plays (a single clip, or a clip in its graph library).

```json
{"method": "anim.retarget_check",
 "params": {"source": 4294967310, "target": 4294967311, "clip": "walk"}}
→ { "ok": true, "mapped_joints": 20,
    "mapped": [ { "role": "chest", "source_joint": "chest", "target_joint": "Bip01 Spine1",
                  "rest_correction_deg": 0.0 }, ... ],
    "unmatched_source_roles": [], "unmatched_target_roles": [], "missing_humanoid_roles": [],
    "scale": 0.8, "root_motion_scale": 0.8, "leg_ratio": 0.8, "facing_yaw_deg": 0.0,
    "clip": { "name": "walk", "frames": 31,
              "max_direction_error_deg": 0.0004, "mean_direction_error_deg": 0.0001,
              "worst_role": "leftLowerLeg", "worst_time": 0.43,
              "source_root_speed": 1.3, "target_root_speed": 1.04, "speed_ratio": 0.8,
              "source_stance_speed": null, "target_stance_speed": null,
              "source_foot_skate": 0.0, "target_foot_skate": 0.0, "goals": [] },
    "issues": [] }
```

### Roles

By default, roles are guessed from joint names. The guesser recognizes these naming styles:

- Mixamo (`mixamorig:LeftUpLeg`);
- game-style (`thigh_l`);
- Biped (`Bip01 L Thigh`);
- deform prefixes (`DEF-thigh.L`);
- camel case (`lShldrBend`);
- generic chains (`leg_joint_L_1..5`).

Twist, IK, end and helper joints never take a role.

Pass `setup` with `.retarget.ron` text to choose roles yourself (`docs/formats/retarget-ron.md`):

```json
{"method": "anim.retarget_check",
 "params": {"source": 4294967310, "target": 4294967311, "clip": "walk",
            "setup": "(format_version: 1, target: (roles: Guess, overrides: { \"leftToes\": \"ball_l\" }), root_motion: Fixed(1.0))"}}
```

### Reading the report

- `mapped[].rest_correction_deg` is how far the target's rest bone had to turn to point where
  the source's rest bone points. An A-pose arm against a T-pose arm is about 40.
- `facing_yaw_deg` is the turn applied when the two rigs were exported facing different ways
  (a multiple of 90).
- `scale` is the target's hip height over the source's. `root_motion_scale` is the scale that
  baked clips apply to root motion (`null` when the setup drops root motion).
- `clip.max_direction_error_deg` is the worst angle between a source bone and its target bone,
  over every sampled frame. A clean retarget is near 0. A `rest_offset_deg` in the setup shows
  up here on purpose.
- `clip.speed_ratio` is the target root speed over the source root speed. It should equal
  `root_motion_scale`. A clip without root motion uses the stance speeds, and the ratio should
  be near `leg_ratio`.
- `leg_ratio` is the target's leg length over the source's. A leg strides with its length, not
  with the hip height. When `leg_ratio` and `scale` differ, root motion scaled by `scale` skates:
  set `root_motion: Fixed(<leg_ratio>)` in the setup.
- `clip.source_stance_speed` and `clip.target_stance_speed` are for clips played in place
  (most imported clips: the importer's root-motion setting is off by default). They hold how
  fast the planted foot slides back, which is the speed the clip walks at. Both are `null` with
  root motion, for an idle, and for a clip whose travel is on the hips.
- `clip.source_foot_skate` and `clip.target_foot_skate` are the horizontal speeds (m/s) of the
  planted foot. A foot counts as planted in frames where the source foot is within 2 cm of its
  lowest point, below the other foot, and nearly still (at most a quarter of the clip's root
  speed plus 0.1 m/s), so the first and last frames of a swing do not count. An in-place clip's
  feet are measured over the ground: the stance speed is taken off.

### IK goals

A setup with `ik_goals` (`.retarget.ron` v2, ADR-8476) is checked the way it bakes: every sampled frame has its
goals solved, so the foot skate and direction numbers above already include them. `clip.goals` has one entry per goal,
in order:

```json
{"method": "anim.retarget_check",
 "params": {"source": 4294967310, "target": 4294967311, "clip": "walk",
            "setup": "(format_version: 2, ik_goals: [(roles: (\"leftUpperLeg\", \"leftLowerLeg\", \"leftFoot\"), mode: Contacts), (roles: (\"rightUpperLeg\", \"rightLowerLeg\", \"rightFoot\"), mode: Contacts)])"}}
→ { ..., "clip": { ..., "goals": [
      { "roles": ["leftUpperLeg", "leftLowerLeg", "leftFoot"], "mode": "Contacts", "weight": 1.0,
        "resolved": true, "max_residual": 0.0001, "mean_residual": 0.00003,
        "planted_frames": 16, "planted_slide": 0.0002 }, ... ] } }
```

- `resolved` is false when the goal's roles are not all mapped on both rigs, or its target joints are not a
  parent → child chain. The bake skips such a goal, and `issues` says why.
- `max_residual` and `mean_residual` are the distance, in meters, from the chain's end to its goal after the solve. A
  residual above 1 cm means the target chain cannot reach where the source put its end.
- `planted_frames` counts the frames in which the source end is planted (the clip's contact curve, or the contact
  labels).
- `planted_slide` (`Contacts` goals) is the largest horizontal distance the target end moves from where it stood at
  the start of a planted run, in meters. It is `null` for `ScaledWorld` goals.

To compare, run the check once with the goals and once without (`ik_goals: []`). Without goals, a rig with longer or
shorter legs than its hip height says shows the slide in `target_foot_skate`.

### Issue codes

`ok` is true when `issues` holds nothing but `facing_turned`.

| Code | Meaning |
| --- | --- |
| `missing_role` | A required humanoid role is not mapped (reported only when the hips are mapped). |
| `large_rest_correction` | A rest correction is above 100°. This is probably a wrong mapping, such as swapped sides. |
| `direction_error` | The worst bone-direction error is above 5°. |
| `root_motion_lost` | The source clip has root motion, but its root-motion joint drives no target joint. |
| `speed_ratio` | The root speed ratio differs from `root_motion_scale` by more than 5 %, or the stance speed ratio from `leg_ratio` by more than 15 %. |
| `foot_skate` | The target's planted foot slides more than the source's (scaled) plus 0.05 m/s. |
| `facing_turned` | Information only: the source was turned to face the target's way. |
| `goal_unmapped` | An IK goal names a role that is not mapped on both rigs. The bake skips the goal. |
| `goal_not_a_chain` | An IK goal's target joints are not a parent → child chain (a twist joint in between). The bake skips the goal. |
| `goal_unreached` | A full-weight IK goal's end stays more than 1 cm from its goal: the target chain cannot reach it. |
| `goal_slide` | A `Contacts` goal's end moves more than 1 cm over a planted run. |

## Observe a crowd

A crowd is the `regia_animation::CrowdAnimation` resource, added by `CrowdAnimationPlugin`. Its
`CrowdAnimator` evaluates many agents on shared gait clips. Game code feeds each agent's position
and velocity with `set_motion`. Rigs with a `CrowdMember` receive their skin palettes from the
crowd.

A gait's `speed` comes from the clip: its root motion, else the hips or an ancestor that travels,
else the stance foot of an in-place clip. A moving clip that shows `speed: 0` here cannot be
measured (usually a rig without guessable legs). Set its speed with
`CrowdArchetype::with_gait_speed`, or the agent plays it at rate 1 with sliding feet.

`planted` is the share of the gait's cycle each foot (left, right) is planted, `null` without
legs. A walk is usually 0.3 to 0.6. A moving gait near 0 has no usable contact windows, so foot
locking does nothing for it.

```json
{"method": "anim.crowd_status", "params": {"agent": 12}}
→ { "agents": 240,
    "archetypes": [ { "name": "mixamo-cm", "joints": 20, "hip_height": 1.0, "has_legs": true,
                      "gaits": [ { "clip": "idle", "speed": 0.0, "cycle": 2.0, "planted": [1.0, 1.0] },
                                 { "clip": "walk", "speed": 1.3, "cycle": 1.0, "planted": [0.55, 0.55] },
                                 { "clip": "run", "speed": 3.2, "cycle": 0.66, "planted": [0.36, 0.36] } ] }, ... ],
    "step": 300, "evaluated_poses": 61, "shared_hits": 74, "updated": 135, "skipped": 105,
    "per_tier": [ 38, 120, 82 ], "locked_feet": 41, "digest": "5be0c1d7a3f09e42",
    "agent": { "id": 12, "archetype": 0, "tier": 1, "gait": 1, "weight": 0.42, "phase": 0.31,
               "rate": 1.0, "speed": 2.1, "yaw": 1.57, "position": [3.2, 0.0, -4.0],
               "hip_ratio": 1.0, "feet_locked": [false, false],
               "feet_planted": [false, false], "evaluated": true } }
```

- `updated` counts the agents due this step. `skipped` counts the agents waiting for their LOD
  interval: tier `k` updates every `lods[k].interval` steps, staggered by agent id.
- `evaluated_poses` counts the poses evaluated this step. `shared_hits` counts the due agents
  that reused a pose evaluated for another agent in the same gait state (tiers with
  `phase_buckets > 0`). On a shared tier, `evaluated_poses < updated` means sharing is working.
- `rate` is the stride-matching playback rate: the rate at which the blended stride covers the
  agent's ground speed, relative to the blended cycle length. It is 1 at a gait's own speed and
  is clamped to `min_rate`..`max_rate`.
- `locked_feet` counts the feet held by foot locking after this step. Agents in exact tiers
  (interval 1, no phase buckets) whose archetype `has_legs` lock a planted foot where it touched
  down, with two-bone leg IK, and fade the lock out over 4 steps once the gait lifts it. An
  agent's `feet_locked` is `[left, right]`; `feet_planted` says which feet its gait plants this
  step (the clips' foot-contact tables, low and still, blended by the gait weight). Shared and
  interval tiers never lock and report no planted feet.
- `digest` hashes every agent's phase, yaw and palette. Two runs with the same inputs give the same
  digest, whatever the chunk size or thread count. To check determinism, wait until `step`
  reaches a value and compare digests.

## Errors

The reason code is in `error.data.code`; the message repeats it as a `[code]` prefix.
`errors.explain { code }` gives the long form of each.

- `F0003`: no such rig, or the entity has no `SkinnedMesh`; no `CrowdAnimation` resource; no such
  crowd agent.
- `unknown_clip`: the source rig does not play `clip` (not found, `F0003`), or `clip` is not a string (bad
  parameters, `F0002`).
- `bad_setup` (bad parameters): `setup` is not a valid `.retarget.ron` document (syntax error,
  unknown `format_version`, a non-positive or non-finite scale, or an unknown built-in profile).
- `missing_joint` (bad parameters): a role in the setup names a joint the skeleton does not have.
- `no_common_roles` (bad parameters): the two rigs share no role.
- `bad_skeleton` (bad parameters): a rig's skeleton has a parent list that does not match its joints
  (a different length, or a parent index past the last joint).
- `bad_agent` (bad parameters): `agent` is not a non-negative integer.
