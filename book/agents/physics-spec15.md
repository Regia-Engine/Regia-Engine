# Physics: debug draw, materials, torque breaks and the physics lab

Four physics features an agent can check without a window. They need `regia_physics::PhysicsPlugin`; without it
the calls return `NOT_FOUND` with a hint. Decision: ADR-2424. Format page: `docs/formats/physmat.md`.

## Parented bodies (the spec 15 § 1 hierarchy warning)

`physics.parented_bodies` answers `{ count, bodies: [{ entity, parent }] }`: the play world's `Dynamic`
`RigidBody` entities parented under an entity with no `RigidBody`. Rapier simulates and writes a dynamic
body's world pose back, but the body's `Transform` is local to a parent the simulation knows nothing about —
the hierarchy and the simulation fight — so the spec recommends bodies as hierarchy roots. `Static` and
`Kinematic` bodies under a group node are the normal, supported case and are not listed. Each offending
entity is logged once (a `tracing::warn!`, visible in the editor's log during play) and counted here;
findings are pruned when the body despawns. The check runs wherever `PhysicsPlugin` runs (a play world — the
editor's edit world has no physics).

## Debug draw

`physics.debug_draw` turns the physics debug draw on or off and chooses what it shows. Call it with no
parameters to read the state.

```json
{ "on": true, "kinds": ["contacts", "joints", "com"] }
→ { "on": true, "kinds": ["contacts", "joints", "com"], "gizmo_group_enabled": true,
    "lines": 0, "collections": 0, "lines_emitted": 0, "dropped": 0 }
```

- `contacts`: each touching contact point with a short line along its normal. `joints`: the body-to-anchor arm
  of both bodies and the gap line between the two anchors (a long gap line is a stretched joint). `com`: a cross at
  the center of mass of every dynamic body. The default is all three.
- Turning it on also enables the `physics` gizmo group, so the lines are visible; that group draws collider
  outlines too. Turning it off leaves the group as it is (use `debug.set_gizmos` for that).
- It costs nothing while off: the collection system does not run, and `collections` stays where it was. After
  turning it on, `play.step` and read the state again: `collections` counts the frames drawn, `lines` is the last
  frame's line count, `dropped` counts lines past the 50,000 per frame cap.
- Errors (the stable code is in brackets at the start of the message): `physics_debug_flag` (`on` is not a
  boolean), `physics_debug_kinds` (`kinds` is not an array of strings), `physics_debug_kind_unknown`.

## Physics materials

A `.physmat.ron` names friction, restitution, density and the combine rules a collider takes
(`docs/formats/physmat.md`).

- `physics.material_check { ron }` validates the text of a `.physmat.ron` and returns a summary. A bad file is an
  `F0002` error whose message starts with the stable code: `physmat_parse`, `physmat_version`, `physmat_name`,
  `physmat_value`.
- `physics.material_register { ron }` validates and adds the material to the registry (the same name replaces).
  Colliders with a `PhysicsMaterialRef("<name>")` component take its values on the next frame. The result adds
  `registered`, the number of materials. Set the component with the usual entity editing methods.
- The default material equals the collider defaults, so a scene without references is bit-identical to before.
  A reference to an unregistered name leaves the collider alone and shows up in `PhysicsMaterials::unresolved`.

## Joint torque break

`JointBreakTorque(n)` next to a `Joint` breaks the joint when its torque, estimated in newton-metres, exceeds
`n` on a step, in addition to `Joint::break_force`. A `JointBroken` message is raised as for a force break, and the
joint stays broken until its `Joint` changes. The estimate adds the moment of the joint's linear reaction about the
joint point to its angular reaction, so a load hanging to the side of a joint counts. Check a break with
`play.step` and `physics.stats` (`joints` drops by one).

## The physics lab

`samples/physics_lab` is a headless, deterministic scene: a stack, a lever joint with a torque limit and one
without, a hanging chain, a default and an ice box on a 20 degree ramp, a ragdoll drop and the debug draw. Run it
with `cargo run -p physics_lab`; the report ends in a digest that two runs share. Its `tests/agent_dogfood.rs`
drives it over this protocol.

```json
{ "method": "play.pause", "params": {} }
{ "method": "physics.debug_draw", "params": { "on": true } }
{ "method": "play.step", "params": { "frames": 3 } }
{ "method": "physics.debug_draw", "params": {} }
```
