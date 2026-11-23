# Character physics: crouch, ceilings, platforms, ragdoll limits and budget

Spec 93 (ADR-1693) polishes the character controller (`regia_framework::character`) and the
ragdolls (`regia_framework::ragdoll`). This page covers what an agent can read and set, and how to
verify the behavior in a running app. Distances are meters, speeds m/s, angles radians. Rigs face
`-Z` by default (`RagdollProfile::forward`).

## Read the characters

`physics.characters` lists every entity that has both `CharacterMovement` and `Transform`, in entity
order. A movement component without a `Transform` is omitted. `entity` limits the list to one
(`character_not_found` when that entity is not in the list).

```json
{ "method": "physics.characters", "params": {} }
→ { "count": 1, "characters": [ {
      "entity": 4294967310, "position": [4.1, 1.2, 0.0], "velocity": [3.0, 0.0, 0.0],
      "grounded": true, "crouched": true, "mode": "walking", "time_in_air": 0.0,
      "ground_velocity": [0.0, 0.0, 0.0], "ground_entity": 4294967296,
      "ground_normal": [0.0, 1.0, 0.0], "ceiling_hit": false, "sliding": false } ] }
```

- `position` is where the **standing** capsule's centre would be, also while crouched, so meshes
  and cameras do not jump. The crouched capsule keeps its bottom on the ground.
- `crouched` stays true after crouch is released while a ceiling leaves no room to stand; the
  character stands up as soon as it has headroom.
- `velocity` is the motion the move achieved: walking into a wall reads near zero, sliding along a
  wall reads the slide. Slopes, stairs and pushing a dynamic body keep the full speed.
- `ground_velocity` is the velocity of the ground under the character (a moving platform). A jump
  or a step off the platform keeps it; landing makes the velocity relative to the new ground.
- `ceiling_hit` is true on a step the head touched something; the upward speed is gone at once.
- A character whose own entity has a `Ragdoll` in `limp` or `active` mode does not move and ignores
  input until the ragdoll recovers.

Verify a crouch under a low bar: hold crouch input, `play.step` a few seconds, then
`physics.characters` shows `crouched: true` and the position past the bar.

## Ragdoll summary

`anim.get_state`, `anim.list` and `anim.ragdoll_set` report each rig's ragdoll with three more
fields:

- `settled`: the limp ragdoll's bodies stayed under `Ragdoll::settle_speed` (linear, m/s) for
  `Ragdoll::settle_time` (seconds, counted in fixed steps). A `RagdollSettled` message fires once per fall, and the
  bodies switch to rest damping so limbs stop rocking; a hit that moves it clears `settled`.
- `face_up`: the settled pelvis' front points up (pick the get-up-from-back clip); false until
  settled.
- `frozen`: the budget froze this ragdoll (kinematic where it lies; the mesh keeps the pose). A mode
  change leaves the freeze.

Verify a fall: `anim.ragdoll_set {entity, mode: "limp"}`, `play.step` about two seconds, then
`anim.get_state {entity}` until the result's `ragdoll.settled` field is true.

## Ragdoll settings and budget

`anim.ragdoll_settings` reads the `RagdollSettings` resource and, with fields, sets it. Every field
is checked before anything changes.

```json
{ "method": "anim.ragdoll_settings", "params": { "max_simulated": 4 } }
→ { "joint_limits": true, "max_simulated": 4, "simulated": 5, "frozen": 1, "settled": 2 }
```

| Field | Meaning |
|---|---|
| `joint_limits` | Build swing-twist joint limits from each bone's `RagdollLimits` (false: unlimited ball joints). Read when a ragdoll's bodies are built. |
| `max_simulated` | Most ragdolls simulated (powered or limp) at once. While more simulate, the ones that went simulated first **and have settled** freeze, oldest first. Unsettled ragdolls never freeze, so the count can exceed the cap until they settle. |

Defaults per quality tier (`RagdollSettings::for_tier`): MobileLow 2, MobileHigh 4, Deck 6, Low 8,
Medium 12, High 16, Ultra 24; joint limits on.

Errors (`F0002` with `data.error_code`):

| Code | Cause |
|---|---|
| `ragdoll_bad_flag` | `joint_limits` is not a boolean. |
| `ragdoll_bad_budget` | `max_simulated` is not a non-negative integer. |
| `character_not_found` | `physics.characters` named an entity that is not in the list: it needs both `CharacterMovement` and `Transform` (`F0003`). |

## Joint limits

`RagdollProfile::humanoid_limited(skeleton, mass, forward)` gives every bone below the hips a
`RagdollLimits { flex, bend, side, twist }`. The joint frame comes from the bind pose: twist along
the bone, bend towards the limb's flex direction (`Forward` for hips, spine, neck, shoulders and
elbows; `Backward` for knees), so positive bend is flexion and the ranges centre on the bind pose.
The physics joint is `regia_physics::JointKind::SwingTwist`.

## Ragdoll profile files

`*.ragdoll.ron` (`format_version` 4) stores a profile: `RagdollProfile::from_ron` reads it,
`RagdollProfile::to_ron` writes it. Versions 1 through 4 load. A newer version is refused with
`ragdoll_format_version`. `ragdoll_parse` is a syntax or shape error. `ragdoll_bad_bone` is an
empty joint name, a non-positive radius or mass, a box with a non-positive width or depth, or a
sole with a non-positive width or height. `ragdoll_bad_active` is an invalid v3 `active` block (a
motor on a joint with no bone, a negative gain, a strength outside `0..=1`, a non-finite number, an
empty get-up clip name; see [active ragdolls](ragdoll-active.md)). Each body's `shape` is `Capsule`, `Sphere`,
`Box(width, depth)` or `Sole(width, height)`.

Version 1 files have no `shape` and load as capsules. Version 2 adds `shape` (ADR-2133 § 6).
Version 3 adds the optional `active` block. Version 4 adds `Sole` and the optional
`active.recovery` block. `regia upgrade-project` rewrites an older file as version 4. The version
2 file below still loads. `to_ron` writes `format_version: 4`, and a sole foot is
`Sole(width: 0.1, height: 0.06)`.

```ron
(
    format_version: 2,
    forward: (0.0, 0.0, -1.0),
    bones: [
        (joint: "hips", radius: 0.12, mass: 10.6, length: None, limits: None, shape: Box(width: 0.3, depth: 0.2)),
        (joint: "head", radius: 0.1, mass: 6.1, length: None, limits: None, shape: Sphere),
        (joint: "calf_l", radius: 0.05, mass: 4.6, length: None,
         limits: Some((flex: Backward, bend: (0.0, 2.3), side: (-0.05, 0.05), twist: (-0.15, 0.15))), shape: Capsule),
    ],
)
```
