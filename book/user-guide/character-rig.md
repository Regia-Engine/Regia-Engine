# Character rigs: twist bones, drivers, pose-space correctives, skeleton profiles

Four pieces that make a skinned character hold up at the joints. All run on the CPU in `regia_animation`, after the
animator evaluates and before the pose is written to the joints. They are data driven by two RON files:
`docs/formats/drivers-ron.md` and `docs/formats/skelprofile-ron.md`.

## Twist and helper bones (C5)

A hand that rolls 90 degrees on one forearm joint pinches the skin at the wrist. Put twist joints along the forearm and let
each carry a share of the roll:

```ron
twists: [(
    name: "forearm_twist", source: "hand", axis: (0.0, 1.0, 0.0),
    driven: [(joint: "twist1", weight: 0.3333), (joint: "twist2", weight: 0.6667)],
)],
```

The roll is measured as the twist of the source's rotation away from its bind rotation about `axis`. Each driven joint
turns about its own copy of the axis by `weight * roll`. The largest step between neighbouring rings drops from the full
roll to a third. If the twist joints are *parents* of the source, set `source_keep` below 1 so the source's own roll shrinks
by the share the chain now carries.

A **helper** joint follows a fraction of another joint's rotation and can slide along a path keyed on that rotation
(shoulder pads, kneecaps). `follow: 0.5` bisects the source's motion.

## Drivers (C4)

A driver reads one channel, maps it through a curve, and writes another. Inputs: an animator parameter, a morph weight,
or a joint's roll, swing or total rotation. Outputs: a morph weight, a material parameter (wrinkle maps), or a joint's
rotation, translation or scale. Drivers run in file order and see each other's morph writes, so one can feed another.

```ron
drivers: [(
    name: "elbow_bulge",
    input: JointAngle(joint: "fore", axis: (1.0, 0.0, 0.0)),
    curve: (keys: [(location: 0.0, value: 0.0, kind: Linear), (location: 100.0, value: 1.0, kind: Linear)]),
    output: Morph("elbow_bulge"),
)],
```

## Pose-space correctives (C4)

Sculpt a corrective morph target at the pose where the mesh collapses (an elbow at 140 degrees) and list the pose
with the weights wanted there. Between listed poses the weights blend smoothly with radial basis functions (Lewis, Cordner &
Fong, SIGGRAPH 2000); at rest the correction is zero and at each listed pose it equals what you authored. These are
authored examples, not learned pose features.

## Attach to a rig

```rust,wrap
use std::sync::Arc;
use regia_animation::{DriverSet, NamedMorphs, RigDrivers, Skeleton};
# use regia_ecs::prelude::World;
# let skeleton = Skeleton::default();
# let mut world = World::new();
# let rig = world.spawn_empty().id();

let set = DriverSet::from_ron_str(&std::fs::read_to_string("hero.drivers.ron")?)?;
// The names of your morph bank, in bank order. NamedMorphs::from_set(&set) is the default
// order (first appearance in the file). Any type can implement `MorphNames`.
let morphs = NamedMorphs::from_set(&set);
let compiled = set.compile(&skeleton, &morphs)?;     // reports every unresolved name at once
world.entity_mut(rig).insert(RigDrivers::new(Arc::new(compiled)));
```

The result lands in the rig's `MorphWeights` (indexed like your names), its joint transforms and skin palette, and
`RigDrivers::state` (driver inputs and outputs, material values). The animator's own pose is not modified.
The morph names are a trait (`MorphNames`) because naming the morph bank is the morph-target lane's job: hand your names in.

## Skeleton profiles (C7)

A `SkeletonProfile` is the canonical joint list of a rig family: names, hierarchy, retarget roles, import aliases, rest
pose rule. Use it to validate imports and to feed the retargeter:

```rust,wrap
# let skeleton: regia_animation::Skeleton = todo!();
use regia_animation::SkeletonProfile;

let profile = SkeletonProfile::vrm_humanoid();       // or generic_humanoid(), or from_ron_str(..)
let report = profile.check_skeleton(&skeleton);      // missing / extra joints, hierarchy, arm elevation
assert!(report.ok(), "{report:?}");
let roles = profile.retarget_profile(&skeleton);     // role -> this skeleton's joint name
// A-pose rig into a T-pose bind: the pose to preview or bake against.
let t_pose = profile.rest_pose_normalization(&skeleton, regia_animation::RestPoseRule::TPose { tolerance_deg: 5.0 });
```

Built-ins: `generic-humanoid` (22 roles, Mixamo aliases) and `vrm` (adds eyes, jaw and finger bones, T-pose within 10
degrees). DO27's 67-joint profile is a `.skelprofile.ron` the DO27 repo owns.

## Agent surface

`anim.rig_attach`, `anim.rig_state` and `anim.profile_check`: see [Character rigs (agents)](../agents/character-rig.md).
