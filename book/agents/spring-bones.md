# Spring bones: hair, tails and cloth strips

Secondary motion for skinned rigs: chains of joints that lag, swing and settle after the animation has posed
the body, and are pushed out of body colliders. The calls need `regia_animation::AnimationPlugin` and a rig with
a `SpringBones` component (built from a `.spring.ron` setup, see `docs/formats/spring-ron.md`).

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_animation::{SpringBoneSetup, SpringBones};
# let rig: Entity = todo!();
let setup = SpringBoneSetup::from_ron_str(&std::fs::read_to_string("fox.glb.spring.ron")?)?;
world.entity_mut(rig).insert(SpringBones::new(setup));
```

The solver runs after the animation and any ragdoll blend, before the joint transforms and the skin palette are
written, and writes **rotations only** (bone lengths stay the animator's). It simulates in world space, so moving or
turning the rig makes a chain swing, in fixed internal steps (60 Hz by default): the result depends on how much
time passed, not on the frame rate.

## Read

`anim.spring_list` returns every rig with spring bones:

```json
{}
→ { "count": 1, "rigs": [ { "entity": 4294967310, "enabled": true, "frozen": false, "reduced": false,
      "step_hz": 60.0, "steps_last_frame": 1, "problems": [],
      "colliders": [ { "name": "head", "joint": "head" } ],
      "chains": [ { "name": "ponytail", "joints": ["tail_0", "tail_1", "tail_2"], "enabled": true,
                    "bound": true, "stiffness": 0.15, "damping": 0.1, "gravity_dir": [0, -1, 0],
                    "gravity_power": 9.81, "hit_radius": 0.02, "colliders": ["head"],
                    "positions": [ [0.0, 1.7, 0.0], [0.0, 1.62, -0.05], [0.0, 1.55, -0.1] ] } ] } ] }
```

`positions` are the chain joints in world space (empty before the first frame, while frozen or when the chain is
unbound). `problems` explains a chain that did not bind: a joint missing from the skeleton, a joint that is not a
child of the previous one, an unknown collider. `frozen` and `reduced` show the distance tier from
`AnimLodCamera`.

## Tune

`anim.spring_set` changes one chain live and keeps its simulation state:

```json
{ "entity": 4294967310, "chain": "ponytail", "stiffness": 0.4, "gravity_power": 4.0 }
```

Fields: `stiffness` (0..1: `1` keeps the animated pose, `0` is a free rope), `damping` (0..1), `gravity_power`
(m/s²), `gravity_dir`, `hit_radius` (metres) and `enabled`. Without `chain`, `enabled` switches the whole
component. Bad values return `INVALID_PARAMS` and change nothing. An entity without `SpringBones` or an unknown
`chain` is `F0003` (not found); the same goes for `anim.spring_reset`.

`anim.spring_reset` re-seeds the chains from the animated pose next frame; call it after teleporting a rig.

## Verify

Step a few frames, then `anim.spring_list` and check `positions` against where the hair should hang, or take a
screenshot (`seeing.screenshot`) and look. For collisions put a collider on the body bone, let the chain settle and
check that every `positions` entry is at least `radius + hit_radius` from the collider's surface. To prove
frame-rate independence, step the same time at two fixed deltas and compare `positions`.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
anim.spring_reset {"entity": 4294967310}   # re-seed the chains from the animated pose next frame
```
