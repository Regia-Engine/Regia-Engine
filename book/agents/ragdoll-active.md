# Active ragdolls: PD motors, hit weakening, balance and get-up

Spec 93 § 10 (ADR-2498) gives `regia_framework::ragdoll` an `Active` mode: every body is dynamic, each joint has a
motor that pulls it towards the animated pose, the feet keep the ragdoll standing, and when it cannot stand it falls,
settles and picks a get-up clip. This page covers what an agent can read and set and how to verify it in a running
app. Units: meters, seconds, radians, kg·m/s for impulses, N·m for torques. Agent guide topic: `ragdoll-active`.

## Switch a ragdoll to active

```json
{ "method": "anim.ragdoll_set", "params": { "entity": 4294967310, "mode": "active" } }
```

Every body turns dynamic, like `limp`. A profile with no driving motor (every `.ragdoll.ron` v1 or v2 file, or a v3 file
with zero gains) is **passive**: an active ragdoll then behaves exactly like a limp one, bit for bit. Switching back to
`kinematic` or `powered` is the get-up: the bodies re-pin to the animation, the rig snaps to where the pelvis lies and
the fallen pose blends out over `blend_time` (the same recover path as from `limp`). A character whose ragdoll is active
does not move.

## Set strength and gains

`ragdoll.active_set` sets the authored group strength and per-joint motor gains. Every field is checked before anything
changes; a rejected call changes nothing.

```json
{ "method": "ragdoll.active_set", "params": {
    "entity": 4294967310,
    "strength": { "arms": 0.4 },
    "gains": [
      { "joint": "thigh_l", "kp": 3600.0, "kd": 12.0, "max_torque": 600.0, "group": "legs", "balance": "hip" },
      { "joint": "foot_l", "kp": 2700.0, "kd": 9.0, "group": "legs", "balance": "ankle" }
    ] } }
```

- `strength`: one number for every group, or `{spine?, arms?, legs?}`, each `0..=1`. It multiplies the gains.
- `gains`: one entry per joint. `joint` must be a ragdoll bone (`ragdoll_unknown_joint` otherwise); a bone without a
  motor gets one (defaults: `group` spine, `kp` and `kd` 0, `max_torque` 100, `balance` none). `kp` and `kd` are `>= 0`,
  `max_torque > 0` (`ragdoll_bad_gain`). `balance` is `none`, `hip` (pelvis to thigh: keeps the pelvis upright over the
  leaning leg) or `ankle` (shin to foot: the foot is a support body and the lean acts there).
- The result is the `ragdoll.state` result.

The motor is a joint drive: an implicit spring-damper with stiffness `kp` and damping `kd` towards the animated pose's rotation
of the bone relative to its parent body, limited to `max_torque` about each axis, solved with the joints and contacts. It
acts equally and oppositely on the bone and its parent, so the motors never spin the ragdoll as a whole. Large stiffness is
stable at the fixed step. `kp` is nominal: the held stiffness is about half of it at the default solver iterations. Keep
`kd` low: the tests use `kp` 2700 to 3600 with `kd` 9 to 12, and 900 with `kd` 90 falls where 900 with `kd` 27 stands.

## Read the state

```json
{ "method": "ragdoll.state", "params": { "entity": 4294967310 } }
→ { "entity": 4294967310, "mode": "active", "built": true, "phase": "active", "drives": true,
    "strength": { "spine": 1.0, "arms": 0.4, "legs": 1.0 },
    "hit_strength": { "spine": 1.0, "arms": 0.62, "legs": 1.0 },
    "motors": [ { "joint": "thigh_l", "group": "legs", "kp": 3600.0, "kd": 12.0, "max_torque": 600.0, "balance": "hip" } ],
    "center_of_mass": [0.0, 1.02, -0.01], "com_offset": [0.0, 0.0, 0.03],
    "support_distance": -0.05, "capture_distance": -0.04, "support_points": 8, "planted_feet": 2, "tilt": [-0.03, 0.0, 0.0],
    "driven_motors": 7, "peak_torque": 84.2, "settled": false,
    "facing": null, "get_up_clip": null, "get_up_clips": ["getup_back", "getup_front"] }
```

- `phase`: `animated` (kinematic or powered), `active` (standing: motors and balance run), `falling` (simulated and
  moving: lost its balance, has no driving motor, or is limp), `passive` (settled), `getting_up` (back to the animation,
  the fallen pose still blending out). Before the bodies are built only `entity`, `mode`, `drives`, `strength`, `motors`,
  `get_up_clips` and `recovery` are reported (`built: false`).
- `hit_strength`: the per-group multiplier after hits (1 = full strength).
- `center_of_mass` (world), `com_offset` (horizontal offset from the center of the feet's support polygon),
  `support_distance` (how far the center of mass is outside the polygon; negative inside; `null` with no foot contact),
  `capture_distance` (the same for the capture point, which also counts the velocity: this is what the fall test reads),
  `support_points` (foot contact points), `planted_feet` (feet locked to fixed ground), `tilt` (the balance lean of the last
  step, axis times angle).
- `driven_motors` and `peak_torque`: motors that drove in the last fixed step and the largest torque.
- `facing` and `get_up_clip`: set when the ragdoll settles (below).

## Hits weaken a group

`anim.ragdoll_hit {entity, joint, impulse}` on a bone with a motor lowers that motor's group: the strength drops to
`1 - |impulse| / full_impulse` (or stays lower if it already is), then rises linearly back to exactly 1 over the
profile's `hit.recover_time`, however deep the drop. A hard hit on an arm makes the arm go limp and come back; a hard
hit on the legs can drop the ragdoll.

## Balance and the fall

Balance runs when a motor has `balance: ankle`. Each fixed step the controller computes the center of mass from the
bodies and the support polygon from the feet's contact points, then leans the legs' target by `cd * offset + cv * velocity`
(clamped to `max_tilt`) so the hips move back over the feet; the offset is measured against the animated pose. With two
feet down, the ankles take the lean front to back and the hips take it sideways. A foot on fixed ground **plants**: it is
locked in place, and its ankle gets the torque limit the sole would set, so a large shove stays in the body. A foot on
something that can move stays free, and that needs more `PhysicsSettings::solver_iterations`.

The ragdoll falls when, for `fall_time`, the capture point is more than `fall_margin` outside the polygon, or no foot touches
anything, or the center of mass has sunk below 0.7 of its animated height over the ankles. The phase turns `falling`, the feet
let go, the motors drop to `fall_strength` (0 by default, passive), and it settles like a limp ragdoll.

## Recovery: stepping, landing, moving supports

`ragdoll.active_set {recovery: "full"}` turns on three things the standing controller does not do (ADR-2963); `"off"` (the
default, and every `.ragdoll.ron` file without a `recovery` block) turns them off, and an object sets single fields over the
current values. **Status:** feet on moving bodies hold (a crate and a rising lift, 3000 steps, under 1 cm of slip) at the default
solver settings. Landing needs **24 or more solver iterations** and stepping **32** (`PhysicsSettings::solver_iterations`; at the
default 4 their outcome flips on small changes): the test biped lands standing from 0.3 m and 1 m drops and, at 32, recovers
sideways shoves from 46 to 55 kg·m/s with one step, which the standing controller loses. Forward and backward shoves past
the standing limit are not reliably recovered (ADR-2963 has the curves), so verify the outcome with `ragdoll.state` rather
than assume it.

```json
{ "method": "ragdoll.active_set", "params": { "entity": 4294967310,
    "recovery": { "max_steps": 3, "landing": true, "moving_support": true, "swing_time": 0.3 } } }
```

- **Stepping.** When the capture point leaves the feet's polygon by more than `trigger_margin`, the ragdoll steps: the foot on
  the side the capture point went (the foot behind for a push straight ahead or back) swings to the capture point plus its
  authored sideways offset plus `overshoot`, at most `max_step` from the other foot and never less than `min_width` across
  from it, lifted `lift` at mid-swing, over `swing_time`. It plants when it touches; if the capture point is still outside,
  it steps again, at most `max_steps` times in one recovery (the count resets after a calm second). Then the usual fall test
  decides. A target the leg cannot reach from its hip is pulled in to what it can. After a step the legs stand in the new
  stance, and while a recovery is under way the joints that carry the body are damped three times their authored `kd`.
- **Landing.** With no foot touching anything the ragdoll is `airborne` (`step_phase`): it does not fall for up to
  `max_air_time`, and the legs reach for the capture point with the knees bent (`pre_bend`). The first touch starts
  `landing`: a crouch `crouch` deep that rises over `absorb_time`, then the standing controller takes over.
- **Moving supports.** A foot whose contacts all belong to one body that can move (a crate, a lift) plants in that body's
  frame and moves with it; a dynamic support gets the foot's share of the weight. The capture point uses the velocity
  relative to the support.

`ragdoll.state` then also reports `step_phase` (`stand`, `swing`, `airborne`, `landing`), `steps` (this recovery),
`total_steps` (since it went active), `swing_target` (where the swing foot will touch down), `support_body` (the entity the
planted feet stand on) and, after a fall, `fall_cause` (`capture`, `no_contact`, `collapse`, `missed_step`). `recovery`
reports the settings.

## Push it

```json
{ "method": "ragdoll.push", "params": { "entity": 4294967310, "impulse": [0.0, 0.0, -60.0] } }
→ { "entity": 4294967310, "body": 4294967320, "joint": "spine", "impulse": [0.0, 0.0, -60.0] }
```

An impulse in kg·m/s on one ragdoll body, applied on the next fixed step: `joint` names the bone (default: the heaviest
body, the chest of a humanoid). For a drop, place the rig in the air (its transform) before switching it to active.

## Get-up choice

When a ragdoll settles, the pelvis' facing is `face_up` (on its back), `face_down`, `side_left` (lying on its left side)
or `side_right`. Among the profile's get-up clips for that facing (all clips if none matches), the one whose key-bone
rotations (relative to the pelvis) are closest to the pose wins; ties go to the first authored. `get_up_clip` reports
it. Play that clip, then `anim.ragdoll_set {mode: "kinematic"}`: the fallen pose blends into the clip.

## Verify

1. `anim.ragdoll_set {mode: "active"}`, `play.step {frames: 30}`, `ragdoll.state`: `phase` is `active`,
   `support_points` above 0, `planted_feet` 2, `support_distance` and `capture_distance` negative.
2. A small shove (`anim.ragdoll_hit` on the chest with a small impulse), `play.step {frames: 300}`: still `active`.
3. A large shove: within a second `phase` is `falling`, later `passive` with `facing` and `get_up_clip` set.
4. `anim.ragdoll_set {mode: "kinematic"}`, `play.step {frames: 2}`: `getting_up`; a few frames later `animated`.
5. Stepping: `ragdoll.active_set {recovery: "full"}`, `ragdoll.push {impulse: [0, 0, -60]}`, `play.step {frames: 120}`:
   `total_steps` is at least 1 (a foot swung and planted); read `phase` and `fall_cause` for the outcome. With
   `recovery: "off"` the same push ends `falling` or `passive` with `fall_cause: "capture"`.

## Errors

| Code | Meaning |
|------|---------|
| `ragdoll_not_found` | The entity has no `Ragdoll` (`ragdoll.state`, `ragdoll.active_set`, `ragdoll.push`). |
| `ragdoll_bad_strength` | A strength is not a number in `0..=1`, or names no group. |
| `ragdoll_bad_gain` | A gain entry has no joint, a negative `kp`/`kd`, `max_torque <= 0`, an unknown group or balance role, or `gains` is not a list. |
| `ragdoll_unknown_joint` | A gain (or `ragdoll.push`'s `joint`) names a joint that has no ragdoll bone. |
| `ragdoll_bad_recovery` | `recovery` is not `"full"`, `"off"` or an object of known fields with valid values (`max_steps` 0 to 16, lengths and times `>= 0`, `pre_bend` 0 to 0.5). |
| `ragdoll_bad_impulse` | `ragdoll.push`'s `impulse` is not three finite numbers, or is longer than 10000 kg·m/s. |
| `ragdoll_not_built` | `ragdoll.push` before the ragdoll's bodies exist (they are built on the next fixed step). |
| `F0003` | The entity does not exist. |

File format: `docs/formats/ragdoll-ron.md` (v4 `active` block). Related: [Character physics](character-physics.md),
[Animation](animation.md).
