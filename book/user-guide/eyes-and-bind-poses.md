# Eye gaze and per-instance bind poses

Two components for characters: `Gaze` makes the eyes follow a target with limits and eyelid follow, and
`InstanceBindPose` gives one avatar its own proportions over a shared skeleton and clip set. Both are added
to the rig entity (the one with `Animator` and `SkinnedMesh`). Neither changes a rig that does not have it.

## Eye gaze

```rust,wrap
use regia_animation::prelude::*;
# use regia_animation::gaze::{Gaze, GazeRig, GazeTarget, LidMorphMap};
# use regia_ecs::prelude::World;
# let skeleton = Skeleton::default();
# let morph_names: Vec<String> = Vec::new();
# let mut world = World::new();
# let player = world.spawn_empty().id();
# let character = world.spawn_empty().id();
let mut rig = GazeRig::from_skeleton(&skeleton)
    .ok_or_else(|| std::io::Error::other("eye joints by name"))?;
rig.convergence = 1.0;       // each eye aims at the target; 0 = parallel eyes
rig.saccade_speed = 9.0;     // radians per second; 0 = instant
let mut gaze = Gaze::new(rig);
gaze.morphs = LidMorphMap::from_arkit_names(&morph_names); // optional: eyeLookUpLeft, ...
gaze.target = GazeTarget::Entity(player);                   // or Point(world), or None
world.entity_mut(character).insert(gaze);
```

- The eyes turn to the target inside per-eye limits (`EyeLimits`, default 35° in, 40° out, 26° up, 31° down). A
  target outside the limits pins the eye at the limit; `result().saturated` says so.
- A new target is reached by a fast turn (a saccade, 9 rad/s by default), not a snap.
- Targets nearer than `min_distance` (0.25 m) are pushed out so the eyes never cross absurdly.
- Lids: add `LidSpec` entries (`upper ≈ 0.7`, `lower ≈ 0.2` of the eye's pitch). They turn on top of the clip's lid
  rotation, so blinks keep working. `GazeRig::from_skeleton` finds `upper_lid`/`lower_lid` joints by name.
- Morphs: the four channels per eye (`eyeLookUp`, `eyeLookDown`, `eyeLookIn`, `eyeLookOut`, 0..1 of the eye limit) go
  to a `GazeMorphSink`. `LidMorphMap` maps them to morph target indices of the rig's `MorphWeights`.
- Head: set `rig.head = Some(HeadFollow { chain, weight })` and the head turns first; the eyes cover the rest.
- `weight` blends the whole effect with the animation (0 = untouched).

## Per-instance bind poses

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_animation::InstanceBindPose;
# let (skeleton, bone_scales, joint_positions, baked_inverse_binds): (regia_animation::Skeleton, std::collections::HashMap<String, f32>, Vec<Vec3>, Vec<regia_core::math::Mat4>) = todo!();
# let character: Entity = todo!();
let tall = InstanceBindPose::uniform(&skeleton, 1.12)?;
let long_legs = InstanceBindPose::from_bone_scales(&skeleton, &bone_scales)?;
let refit = InstanceBindPose::from_joint_positions(&skeleton, &joint_positions)?
    .with_inverse_binds(baked_inverse_binds)?; // the frame the baked mesh was skinned in
commands.entity(character).insert(tall);
```

The clips and graph stay as authored for the shared skeleton. The final pose moves onto the instance's rest pose:
rotations keep their offset from rest, translations keep their offset scaled by the bone length, so hip bobs and
strides scale with the legs. Joint transforms, sockets, joint globals and the skin palette all follow the instance.
The shared `Skeleton` asset is never edited. Root motion is not scaled; a body whose legs differ from the shared
skeleton at the same character speed slides a little, which `foot_ik` corrects. 100 avatars of 60 joints retarget and
build palettes in well under the 2 ms budget (see the agent page for the measurement).
