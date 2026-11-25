# Move a character

<!-- index: Character -->
<!-- index: Character controller -->
<!-- index: Player controller -->
<!-- index: Movement mode -->
<!-- index: Coyote time -->
<!-- index: Root motion -->

A character in Regia is a capsule that moves through the physics world. You write
`CharacterInput` each frame. `CharacterPlugin` turns that into a step, and
`CharacterState` is what animation and gameplay read back. This page is the
procedure. The reference is [Gameplay](../../user-guide/gameplay.md) and
[Physics](../../user-guide/physics.md). The design is
`docs/specs/14-scene-and-framework.md` (the framework) and
`docs/specs/15-physics.md` § 4 (the controller), with the
capsule rules in `docs/specs/93-character-physics.md`.

## Start from the third-person template

The shortest path to a walking character is the template. It already adds
`PhysicsPlugin`, `CharacterPlugin`, and the rapier controller.

```text
regia new "My Game" --template third_person
cd my_game
cargo run -p my_game_launcher
```

Controls in that project: WASD move, Space jump, mouse look, J or left click
attack, F5 quicksave, F9 quickload, Esc pause. The title screen has Play,
Continue, Settings, and Quit. What each file owns is on the
[Third Person template](../../user-guide/third-person-template.md) page.

`cargo test -p my_game` runs the template's headless smoke checks, including a
scripted move.

## Assemble one in your own app

`DefaultPlugins` does not install physics. Without both plugins below, the
capsule does not resolve against colliders: `CharacterPlugin` alone uses a flat
plane, and the rapier backend warns and leaves the character unmoved when
`PhysicsPlugin` is missing.

1. Add `regia_physics::PhysicsPlugin` and
   `regia_framework::character::CharacterPlugin`.
2. Call `regia_framework::character::use_rapier_character_controller` so the
   step uses rapier's kinematic controller (autostep, slope limits,
   snap-to-ground, moving platforms, pushing dynamic bodies).
3. Spawn an entity with `Character`, `CharacterMovement`, and a `Transform`.
   The default capsule is radius 0.3 m and half-height 0.9 m. Its center sits
   at standing height. Crouch shrinks the capsule from the top. The transform
   stays at the standing center, and the character stands up only when there
   is headroom.
4. Each frame, write `CharacterInput`: `move_dir` in world space, and `jump`
   when the player asks to jump. A `PlayerController` possesses the character
   for a human. An `AiController` does the same for a bot. See
   [Gameplay](../../user-guide/gameplay.md#characters).

Bind the keys in an `InputConfig` the way the template does (`Move` on WASD,
`Jump` on Space and the gamepad south button). Reading actions is on the
[Input](../../user-guide/input.md) page. For a first-person look, also set the
cursor grab described there. Locked falls back to Confined on Windows and X11.
Confined falls back to Locked on macOS.

## What the step does

`CharacterPlugin` steps in `FixedUpdate`, after physics. Modes are
`Walking`, `Falling`, `Flying`, `Swimming`, and `Custom`.

- Walking uses ground braking when there is no input. `ground_friction` is
  stored and is not applied by the walker.
- Falling keeps air control. A jump is still accepted for a short time after
  walking off a ledge (coyote time). A jump pressed just before landing fires
  on touchdown (jump buffer). `max_jumps` allows extra jumps in the air.
- A ceiling ends a jump at once.
- The character rides a moving platform, including that platform's yaw about
  the spin axis. It does not turn the character's facing to match the yaw.
- Over kinematic ground the move has two parts: first with the ground, then
  the character's own move, so the ground's motion is not added twice.
- Pushing a dynamic body applies an impulse from the character's mass.
- `CharacterState` reports the motion that actually happened: velocity,
  `grounded`, `mode`, and time in the air. Animation should read this, not the
  input.

Slope and stair checks live in `regia_framework/tests/character_physics.rs`
and `regia_physics/tests/character_controller.rs`: the controller reaches the
top of the stairs, does not climb a 60° slope, and rides a moving platform
with jitter under 1 cm. One-way triggers are not built.

## Drive the body you see

`CharacterState` is the bridge to the rig.

- Play a locomotion clip or an anim graph from `grounded` and speed. See
  [Animation](../../user-guide/animation.md).
- For captured locomotion without a hand-built state machine, use
  [Motion matching](../../user-guide/motion-matching.md) at runtime
  (`TrajectoryPredictor` plus `MotionMatchCharacter`). The editor panel that
  inspects a database is a separate page.
- Root motion, when a clip provides it, is added after the physics step. Mark
  the character `RootMotionDriven` when the clip should move the capsule. See
  [Animation](../../user-guide/animation.md#root-motion-on-characters).
- Attacks and vaults are montages with motion warping, not a change to the
  walk cycle. See [Montages](../../user-guide/montages.md) and
  [Combat and abilities](combat-abilities.md).
- A ragdoll takes the rig when the character goes limp. The capsule waits.
  Joint limits come from the bind pose. See
  [Physics](../../user-guide/physics.md#characters).

## Lockstep games

A lockstep simulation does not read a rapier pose, contact, or character
sweep. Move those units with the models in `regia_detsim`. The table is on
[Movement models](../../user-guide/movement-models.md). The rapier character
on this page is for games that present physics, and for the local player of a
client-predicted game that does not feed rapier back into the lockstep.

## Finished check

You are done when all of these are true in one play session:

- The capsule walks on static ground and stops when you release the move input.
- It climbs a stair whose step is inside the autostep height, and it does not
  walk up a 60° slope.
- It jumps, lands, and `CharacterState.grounded` is true again on landing.
- It rides a moving platform without sliding off, and a jump off that platform
  keeps the platform's velocity.
- Crouch lowers the sweep and refuses to stand while a ceiling is in the way.
- The mesh or anim graph follows `CharacterState` rather than the raw keys.

Headless, the same facts are what `character_physics.rs` and the template's
`cargo test -p my_game` assert. An agent can `play.inject_input` the move
keys and `play.wait_until` on `CharacterState`.

## When it does not work

- The character sinks through the floor or ignores walls. `PhysicsPlugin` is
  missing, or `use_rapier_character_controller` was not called, so the flat
  plane is still the query.
- The character never leaves the ground. `jump` is written while `grounded`
  is already false and both the buffer and coyote windows have closed, or
  `move_dir` is set and `jump` is cleared in the same system after the
  controller sampled it.
- Crouch stands the capsule into a ceiling. Stand-up is gated on headroom.
  If your ceiling collider is a sensor, it does not block the check.
- Animation slides. The graph is playing a loop whose root motion is also
  applied, or it is not reading `CharacterState` velocity.

## Where to read next

- [Physics](../../user-guide/physics.md) for bodies, queries, and
  [destruction level 2](../../user-guide/physics.md).
- [Continuous collision detection](../../systems/physics-ccd.md) for fast
  movers. The character sweep is not a substitute for `SweptCcd` on a bullet.
- [Navigation and AI](../../user-guide/navigation-ai.md) when a bot should
  path to the character instead of holding a stick deflection.
- [Input](../../user-guide/input.md) for rebinding and the cursor.
