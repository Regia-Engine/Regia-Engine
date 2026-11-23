# Spec 93: Character controller polish and ragdolls

Status: proposed (ADR-1693). Plan item "character controller polish, ragdolls", rung v0.3.x (b)
(`docs/master-plan.md` § physics rows, `docs/versions.md` v0.3.x line (b)). No reviewed public-spec set covers this item:
the Regia docs (specs 14 § B4, 15 § 2/§ 4, 17 § 4) and the public papers cited in `docs/provenance.md` are the source.

## 1. Purpose

**Baseline (built before this spec).** `regia_physics::character::move_character` wraps rapier's kinematic character
controller (autostep with the ADR-1387 stair fix, slope limits, snap-to-ground, platform riding, push impulses).
`regia_framework::character` runs the movement state machine (`CharacterMovement`, `CharacterInput`,
`CharacterState`, coyote time, jump buffer, double jumps, root motion). `regia_framework::ragdoll` builds capsule bodies
per bone with unlimited ball joints and drives them `Kinematic` / `Powered` / `Limp`, with hits, momentum hand-off and a
frozen-pose recover. The agent has `anim.ragdoll_set` and `anim.ragdoll_hit`.

**Gaps this spec closes.**

1. Crouch only slowed the character: the capsule never shrank, so a crouching character could not pass under a low
   ceiling, and nothing stopped it standing up into one.
2. A head hit kept the upward velocity: a jump under a ceiling stuck to it until gravity won.
3. Leaving a moving platform lost the platform's velocity: a jump from a lift or a train stopped dead in the air.
4. `CharacterState::velocity` kept the input speed while a wall blocked the move, so locomotion blend spaces ran in
   place against walls.
5. A limp ragdoll on a character kept the character's capsule walking under player input.
6. Ragdoll joints had no limits (ADR-1563 j): knees bent forwards and heads spun.
7. Nothing told gameplay that a fallen ragdoll had come to rest, or whether it lay face up or face down (to pick the
   get-up clip).
8. No budget: every limp ragdoll simulated forever; mobile tiers need a cap.
9. Ragdoll profiles were code only; a designer could not author one as a file.

## 2. API shape

### 2.1 `regia_physics` (joints)

New `JointKind` variant:

```rust
JointKind::SwingTwist {
    twist_axis: Vec3,     // this body's local space (the bone direction)
    bend_axis: Vec3,      // this body's local space; made perpendicular to twist_axis
    bend: (f32, f32),     // rotation about bend_axis, min/max (rad)
    side: (f32, f32),     // rotation about twist_axis × bend_axis, min/max (rad)
    twist: (f32, f32),    // rotation about twist_axis, min/max (rad)
    rest: Option<Quat>,   // connected⁻¹ · this body rotation at which every angle is zero
}
```

The angles are the rotation of **this** body relative to the connected body, measured from `rest`, positive by the
right-hand rule about each axis. `rest: None` takes the relative rotation the two bodies have when the joint is created
(like `Fixed`). The joint is a ball joint (anchors coincide) with three per-axis angular limits; the swing limit is the
bounding box of the two swing ranges (exact along each axis, wider on the diagonals, as the existing `cone_limit`).

### 2.2 `regia_framework::character`

- **Crouch.** While `CharacterInput::crouch` is held the swept capsule has `CharacterMovement::crouch_half_height`, its
  bottom where the standing capsule's bottom is. The entity's `Transform` stays where the *standing* capsule's centre
  would be, so meshes and cameras do not jump; `CharacterState::crouched` reports the state. Releasing crouch stands
  up only when the capsule has room: `CollisionQuery::has_headroom` (new trait method with a default of `true`; the
  rapier backend casts the crouched capsule up by the height difference). A character kept down by a ceiling stays
  crouched until it moves out from under it.
- **Ceiling.** `MoveResult::ceiling_hit` (new field). A head hit while moving up zeroes the upward velocity.
- **Platform momentum.** `MoveResult::ground_velocity` (new field; the rapier backend fills it with the ground body's
  velocity at the character). Leaving the ground (a jump or walking off) adds the last ground velocity to the
  character's velocity; landing subtracts the new ground's (the backend adds ground motion itself while grounded).
  `CharacterState::ground_velocity` reports it.
- **Achieved velocity.** After the move the horizontal velocity is limited to the horizontal motion the move achieved
  (minus the ground's): pushing into a wall reads as standing; sliding along a wall reads as the slide. It applies to a
  grounded move (grounded before and after) that achieved less than 90 % of the asked distance and pushed no dynamic
  body: slopes and steps keep the speed, a push keeps the push, and the air keeps its momentum.
- **Ragdoll hand-off.** A character entity whose `Ragdoll` is `Limp` (or `Active`, § 10) does not move: its velocity is
  zeroed and input is ignored until the ragdoll recovers (the recover snap places the rig where the body fell).

### 2.3 `regia_framework::ragdoll`

- `RagdollBone::limits: Option<RagdollLimits>` with `RagdollLimits { bend, side, twist }` (min/max pairs, rad).
  The joint frame is built from the **bind pose**: twist = bone direction, bend axis = `bone × flex_toward` where
  `flex_toward` is the rig-space direction the limb flexes towards (`RagdollLimits::flex`: `Forward` or `Backward`
  relative to `RagdollProfile::forward`). Positive bend = flexion. Built into `JointKind::SwingTwist` with `rest` from
  the bind pose.
- `RagdollProfile::forward: Vec3` (rig space; the way the character faces, default `-Z`, the Regia character
  convention) and `RagdollProfile::humanoid_limited(skeleton, mass, forward)`: the 11-bone humanoid with limits rounded
  from clinical range-of-motion tables. `humanoid` keeps unlimited joints (unchanged behaviour).
- **Settle.** A `Limp` ragdoll whose every body stays under `Ragdoll::settle_speed` (m/s, linear) for
  `Ragdoll::settle_time` seconds (counted in fixed steps) is *settled*: `RagdollBodies::settled()` turns true and one
  `RagdollSettled { rig, face_up, root_position }` message is written. `face_up` is true when the pelvis body's forward
  axis (the profile's `forward` carried by the pelvis rotation) points up. Spin is not counted: capsules roll on the
  floor (there is no rolling friction), so a limb can rock in place for ever with almost no linear speed. A settled
  ragdoll's bodies get **rest damping** (linear 2, angular 8) that stops the rocking; moving again faster than twice
  `settle_speed` (a hit, a push) or a mode change clears the settle and restores the moving damping (0.1, 0.5).
- **Budget and switch.** Resource `RagdollSettings { joint_limits: bool, max_simulated: usize }`, defaults from
  `RagdollSettings::for_tier(QualityTier)` (MobileLow 2, MobileHigh 4, Deck 6, Low 8, Medium 12, High 16, Ultra 24).
  `joint_limits = false` (a runtime switch) builds unlimited ball joints (the old behaviour; read when a ragdoll's
  bodies are built). When more ragdolls are simulated (`Powered`, `Limp` or `Active`, § 10) than `max_simulated`, the ragdolls that
  went simulated earliest and have settled are **frozen**: their bodies turn kinematic where they lie and the mesh keeps
  the fallen pose. A frozen ragdoll that changes mode leaves the freeze. Ragdolls that have not settled are never
  frozen (no corpse stops in mid-air); the cap can be exceeded until they settle.
- **Profile file.** `*.ragdoll.ron` (`RagdollProfileFile`, `format_version: 4` today, § 3; it was 2 when this section
  was written), loaded by `RagdollProfile::from_ron`, written by `RagdollProfile::to_ron`. Unknown newer versions are an
  error (`ragdoll_format_version`).

## 3. Files and formats

`*.ragdoll.ron`, `format_version: 4` (v3 adds the optional `active` block, § 10; v4 the `Sole` shape and `active.recovery`, ADR-2963; each body's `shape`: `Capsule`, `Sphere` or `Box(width, depth)`, ADR-2133 § 6;
full schema in `docs/formats/ragdoll-ron.md`):

A version 2 file (still read, and migrated on load; a file written today says `format_version: 4`):

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

**Migration note.** Version 2 adds the per-bone `shape`. A version 1 file has none and loads with every body a capsule;
the 1 → 2 migrator (`regia_framework::ragdoll::file::migrate_ragdoll`) writes `shape: Capsule` on every bone, and
`regia upgrade-project` runs it. The same function carries 2 → 3 (only the version: zero gains write no `active` block,
§ 10) and 3 → 4 (only the version: `Sole` and `active.recovery` are new and optional, ADR-2963); it reads versions 1,
2 and 3. The lead re-registers the format with `cargo xtask check-formats --update`.

## 4. Agent-protocol surface

- `physics.characters { entity? }` (read): every character in entity order: position, velocity, grounded, crouched,
  mode, ground entity, ground normal, ground velocity, ceiling hit, sliding.
- `anim.ragdoll_settings { joint_limits?, max_simulated? }`: reads, and with fields sets, `RagdollSettings`; the
  result counts simulated and frozen ragdolls. Errors: `ragdoll_bad_budget` (not a non-negative integer),
  `ragdoll_bad_flag` (not a boolean).
- The ragdoll summary of `anim.list` / `anim.get_state` / `anim.ragdoll_set` gains `settled`, `face_up` and `frozen`.

Guide page: `book/agents/character-physics.md`; `agent.guide` topic `character-physics`.

## 5. Determinism

Everything here runs in `FixedUpdate` in entity order. The settle timer counts fixed steps, never wall time. The
budget orders ragdolls by the fixed step they went simulated and then by entity bits, so the same inputs freeze the same
ragdolls. The movement step stays a pure function of `(state, input, dt)` plus the collision backend.

## 6. Tests and acceptance

- `regia_physics/tests/swing_twist.rs`: a body hanging from a static anchor under a torque stops at the bend maximum,
  and the opposite torque stops at the minimum (the sign convention); the twist limit holds; `rest` re-centres the
  ranges.
- `regia_framework/tests/character_polish.rs` (rapier backend): crouch passes under a bar a standing capsule cannot;
  standing up under the bar is refused and succeeds once clear; a jump into a ceiling loses its upward speed at once; a
  jump from a moving platform keeps the platform's speed in the air; walking into a wall reads a near-zero velocity; a
  limp ragdoll character ignores input.
- `regia_framework/tests/ragdoll_polish.rs` (it grew out of `tests/ragdoll.rs`): limits keep a limp humanoid's knees from bending forwards; a ragdoll
  pushed backwards settles face up, pushed forwards face down, with exactly one `RagdollSettled` message, and six
  shoves (three strengths each way) all settle within 6 s and stay settled; the budget
  freezes the oldest settled ragdoll and never an unsettled one; `joint_limits = false` builds unlimited joints; a
  settled ragdoll has the rest damping and a hit restores the moving damping. The profile file round-trips and rejects a
  future version (unit tests in `src/ragdoll/file.rs`).
- Agent: `physics.characters` and `anim.ragdoll_settings` unit tests and the coverage call list.
- Perf (ignored, prints): `perf_ragdolls_step` reports the fixed-step cost of 16 limp humanoids with and without
  limits.

## 7. Platform notes

Pure CPU code, no platform seams: Linux, Windows and Android aarch64 build the same code (`cargo ndk -t arm64-v8a check
-p regia`). The mobile tiers get the smaller budget through `RagdollSettings::for_tier`.

## 8. Performance budget

A limp humanoid ragdoll is 11 bodies and 10 joints. Budget: 16 simulated ragdolls ≤ 1.0 ms of physics step on the
desktop reference; 4 ≤ 0.5 ms on mobile. Joint limits add three constraint rows per joint while active. Settled bodies
sleep in rapier and cost little; the freeze removes them from the solver entirely. Measured (the build server, `--release`,
host at load ~100, `perf_ragdolls_step`): 16 limp humanoids falling 0.72 ms mean / 1.25 ms peak with limits (0.47 /
0.87 without), resting 0.10 ms (0.05 without). An idle desktop reference and a mobile device run are still owed.

## 9. Out of scope

Walking (a gait cycle; § 10 steps only to recover a push, ADR-2963; `Powered` keeps velocity tracking); ragdolls on 2D
physics; network replication of ragdoll bodies (games replicate
the death event, each client simulates); exact elliptical swing cones.

## 10. Active ragdoll (addendum, ADR-2498, ADR-2564)

Moves active balance, get-up selection and per-bone PD motors into scope (they were in § 9).

- **Mode.** `RagdollMode::Active`: every body dynamic, blend 1, motors and balance on. With no driving motor (all gains
  zero: every v1/v2 file) it is exactly `Limp`, bit for bit. `Active → Kinematic/Powered` is the recover path (§ 2.3).
  A character whose ragdoll is `Active` does not move, as for `Limp`.
- **Motors.** `RagdollProfile::active.motors`, one `RagdollMotor { joint, group, kp, kd, max_torque, balance }` per bone.
  Each fixed step the child body's rotation relative to its parent body is driven towards the animated pose's relative
  rotation by a **joint drive**: rapier's implicit, force-based angular motors (`regia_physics::pd::set_joint_drive`),
  stiffness and damping times the group strength, torque limited per axis. It acts equally and oppositely on both bodies
  and the solver is unchanged. `kp` is the spring's nominal value; the held stiffness is a fraction of it (about half at
  the default iterations, ADR-2564), and a low `kd` keeps more of it.
- **Groups and hits.** Groups `Spine`, `Arms`, `Legs`; authored `active.strength {spine, arms, legs}` (0..=1). A
  `RagdollHit` on a motored bone drops its group to `min(now, 1 - |impulse| / hit.full_impulse)`, recovering linearly
  to exactly 1 after `hit.recover_time` (fixed steps).
- **Balance** (SIMBICON-style, Yin, Loken and van de Panne, SIGGRAPH 2007). Runs when a motor has `balance: Ankle`.
  Center of mass from the bodies; support polygon = convex hull (x/z) of the `Ankle` bodies' touching contact points
  against anything outside the rig. Lean `cd * d + cv * v` clamped to `max_tilt`: `d` the center of mass over the ankles
  less the same offset in the animated pose, `v` its velocity, both horizontal. With two feet down the lean splits along
  the line between them: across it the ankles lean; along it the hips lean and the ankle only keeps the foot flat. The hips
  hold the pelvis' world rotation about the line (the torso servo); with one foot down they hold it about every axis.
- **Planted feet.** A foot whose contacts all belong to fixed bodies is locked in place while the ragdoll stands, so the
  rig holds at the default solver iterations. Its ankle torque is limited about each world axis to the foot's weight share
  times the lever to its farthest contact point, the hips' to the weight share times the polygon's reach, so a shove stays
  in the body. A foot on a body that can move stays free: raise `PhysicsSettings::solver_iterations` for those.
- **Fall.** The capture point `c + v sqrt(h / g)` more than `fall_margin` outside the polygon, or no foot contact, or the
  center of mass under 0.7 of its animated height over the ankles, for `fall_time`: the ragdoll falls (feet released, motors
  at `fall_strength`, default 0) and settles like a limp one.
- **Get-up.** On settle: facing `FaceUp | FaceDown | SideLeft | SideRight` from the pelvis' forward and left axes (the
  more vertical decides), then among `active.get_up` clips of that facing (all if none) the smallest sum of squared
  key-bone angle errors (rotations relative to the pelvis); ties to the first. Reported as state; the game plays the
  clip and switches the mode.
- **State.** `RagdollBodies::active()` → `RagdollActiveState`: phase `Animated | Active | Falling | Passive |
  GettingUp`, per-group hit strength, center of mass, COM offset, support and capture distance, the support point count,
  planted feet, lean (`tilt`), motors driven (`driven_motors`), peak torque, facing, chosen clip. It runs inside `drive_ragdolls` (no new fixed system, no new component: § 5).
- **Format.** `.ragdoll.ron` v3: optional `active` block (`docs/formats/ragdoll-ron.md`), every sub-block defaulted.
  The 2 → 3 migrator only bumps the version (zero gains write no block). Error `ragdoll_bad_active`.
- **Agent.** `ragdoll.state {entity}` (read) and `ragdoll.active_set {entity, strength?, gains?}`; `anim.ragdoll_set`
  takes `mode: active`. Codes `ragdoll_not_found`, `ragdoll_bad_strength`, `ragdoll_bad_gain`, `ragdoll_unknown_joint`.
  Guide `book/agents/ragdoll-active.md`, topic `ragdoll-active`.
- **Tests** (`regia_framework/tests/ragdoll_active.rs`, `regia_physics/tests/joint_drive.rs`, fixed steps): a 3-body chain
  tracks a static pose within the ADR's bound (0.01 rad after 120 steps); motors conserve angular momentum; zero gains are
  bit-identical to `Limp`; a standing biped holds 600 steps under a small push and falls under a large one, in every direction,
  and holds 3000 steps with no push; a foot plants only on fixed ground; a hit on the legs collapses it; leaving `Active`
  clears drives and locks; hit strength is back to 1 after the authored time; a prone pelvis picks the face-down clip; two runs
  give the same digest; ignored bench `perf_active_ragdolls_step` (20 active bipeds).
- **Determinism.** Everything runs in `FixedUpdate` in entity and body order; contact points are hulled after a total
  sort, so their narrow-phase order does not matter.
- **Recovery** (ADR-2963, `regia_framework::ragdoll::stepping`, `RagdollActiveProfile::recovery`, `.ragdoll.ron` v4
  `active.recovery`; off by default, `RagdollRecovery::full()` on). *Stepping*: when the capture point leaves the polygon
  by `trigger_margin`, a swing foot (the side the capture point went; the foot behind for a push straight ahead or back)
  steps to the capture point plus its authored sideways offset plus `overshoot`, clamped to `max_step` and `min_width`,
  on a fixed-step timeline (`swing_time`, `lift`), joint-space targets from two-bone inverse kinematics; it plants on
  touch-down and steps again while the capture point is outside, at most `max_steps` per recovery; afterwards the legs
  stand in the stepped stance. *Landing*: `Active` with no foot contact is airborne (no fall for `max_air_time`), the
  legs reach for the capture point with bent knees, the touch-down crouches `crouch` and rises over `absorb_time`.
  *Moving supports*: a foot on one body that can move is locked and carried in that body's frame (its velocity at the
  foot plus a drift correction), a dynamic support gets the weight share, the capture point uses the relative velocity.
  State adds `step_phase` (`stand | swing | airborne | landing`), `steps`, `total_steps`, `swing_target`,
  `support_body`, `fall_cause` (`capture | no_contact | collapse | missed_step`). `RagdollShape::Sole` is a level foot
  for imported foot bones; `RagdollProfile::humanoid_with_feet` adds soles on the rig's foot joints and
  `RagdollProfile::humanoid_active` gives a humanoid motors and balance roles. Agent: `ragdoll.push`,
  `ragdoll.active_set {recovery}`. Tests: `regia_framework/tests/ragdoll_stepping.rs`. Implementation status (ADR-2963):
  moving supports meet their acceptance at the default 4 solver iterations (crate 0.0 mm, rising lift 8 mm of foot slip over
  3000 steps; the tests bound the slip at 2 cm). Landing needs 24 or more solver iterations and stepping 32: both drops
  (0.3 m, 1 m) land standing, and at 32 sideways shoves from 46 to 55 kg·m/s that topple the standing controller are
  recovered with one step in the ADR's sweep (the acceptance test checks 46 to 54 and allows up to three steps); forward and backward
  stepping do not meet their acceptance (forward outcomes above the standing limit are not monotonic; that test is ignored
  with the ADR as the reason). The swing target is pulled into the leg's reach from its hip, and while a recovery is under
  way the body-carrying joints get three times their authored damping. CesiumMan (`humanoid_active`, sole feet) stands 3000
  steps and takes a push with the standing controller at 24 iterations (it falls at 4 to 16).
