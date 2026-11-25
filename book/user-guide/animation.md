# Animation

<!-- index: Animation -->
<!-- index: Anim graph -->
<!-- index: Animation clip -->
<!-- index: Root motion -->
<!-- index: Foot IK -->
<!-- index: Retargeting -->
<!-- index: Ragdoll -->
<!-- index: Spring bone -->
<!-- index: Inertialization | see: Anim graph -->

`regia_animation` imports glTF clips, plays skeletal animation, and runs **anim graphs** —
state machines, blend spaces and layers that decide what plays. The model is
[spec 17](../../docs/specs/17-animation.md). One-shot actions are
[Montages](montages.md) ([spec 34](../../docs/specs/34-montage-extensions.md)).
Runtime motion matching is [Motion matching](motion-matching.md)
([spec 52](../../docs/specs/52-motion-matching.md)). Crowd retargeting is
[spec 85](../../docs/specs/85-retarget-crowds.md). A walking character that
consumes `CharacterState` is [Move a character](../how-to/play/character.md).

## Importing clips

glTF skins and animation clips cook into `Skeleton` and `AnimationClip` assets. Drop a skinned
`.glb` under `assets/` and it imports; the mesh, skeleton and clips are separate cooked assets
referenced from the mesh. The bind mesh may be named `_LODn` with `n > 0` (a clip file that holds
one LOD). The cook still writes the skeleton and the clips, and that mesh is the primary, which is
the clip set's bind mesh. A file with no triangle mesh still fails with `no mesh found in glTF file`.

## Playing a clip

```rust
use regia::prelude::*;
use regia_animation::prelude::*;

fn start_run(mut commands: Commands) {
    // In a game the clip comes from the cooked asset; build one here for the example.
    let clip = AnimationClip::empty("Run", 1.0);
    // The rig entity also carries a `SkinnedMesh` and a skinned `MeshRenderer`.
    commands.spawn(Animator::single(clip, true));
}
```

`Animator` drives the skeleton each frame; `Animator::single(clip, looping)` plays one clip,
`Animator::graph(graph, clips)` runs an anim graph. Crossfade and blend between states in the graph
(`Animator::set` / `Animator::trigger` set parameters and transitions).

## Anim graphs

For characters that combine states (idle, run, jump) with blend spaces and per-bone layers, author
an `.animgraph.ron` (frozen; v2 adds [inertialized transitions](inertialization.md), v3 adds
the [montage `Slot` node](montages.md), and v1 and v2 files still load;
`docs/formats/locomotion.animgraph.ron`). A `format_version`, a
`parameters` table, and a `root` node that nests state machines, blend spaces and layers:

```ron,animgraph
(
    format_version: 1,
    parameters: [
        ("Speed", Float(0.0)),
        ("Land", Trigger),
        ("WaveWeight", Float(0.0)),
    ],
    root: Layer((
        base: StateMachine((
            initial: "Start",
            states: [
                ("Start", Clip((clip: "WalkStart", looping: false, speed: 1.0, sync_group: None))),
                ("Loop", BlendSpace1D((
                    space: (
                        param: "Speed",
                        samples: [
                            (value: 0.0, clip: "Idle"),
                            (value: 1.6, clip: "Walk"),
                            (value: 6.0, clip: "Run"),
                        ],
                    ),
                    sync_group: Some("Locomotion"),
                ))),
                ("Land", Clip((clip: "WalkLand", looping: false, speed: 1.0, sync_group: None))),
            ],
            transitions: [
                (
                    from: Some("Start"),
                    to: "Loop",
                    conditions: [ClipFinished],
                    duration: 0.15,
                    curve: Smooth,
                    interruptible: false,
                ),
                (
                    from: Some("Loop"),
                    to: "Land",
                    conditions: [Trigger(param: "Land")],
                    duration: 0.2,
                    curve: Linear,
                    interruptible: false,
                ),
            ],
        )),
        layers: [
            (
                graph: Clip((clip: "Wave", looping: true, speed: 1.0, sync_group: None)),
                mask: (joints: ["Spine", "Arm_R"], include_children: true),
                mode: Override,
                weight_param: "WaveWeight",
            ),
        ],
    )),
)
```

The graph evaluates parameters (like `speed`) each frame and picks the active state. The schema is
`docs/formats/animgraph.schema.json`.

## Foot IK

`regia_animation::foot_ik` (M20) is the foot-planting pass for characters on slopes and stairs:
`plan_foot_ik` turns ground contacts into a pelvis adjustment plus one two-bone IK target per leg
(which the graph's `TwoBoneIK` nodes consume), and `solve_foot_ik` applies a plan directly to a
pose. The `samples/third_person` fox drives it through the anim graph — see the
[agent guide's gameplay page](../agents/gameplay.md) for the `SolvedFoot` acceptance anchor.
For a planter that keeps feet pinned while they are planted, handles stairs with heel and toe
probes and runs without glue code, use the `FootPlacement` component instead (see
[Foot placement with FootPlacement](#foot-placement-with-footplacement) below).

## Chain IK: FABRIK and CCD

For spines, tails, cables and multi-segment arms, `regia_animation::chain_ik` solves chains of
any length. Build the chain from two joint names, optionally attach per-joint limits (a cone with
a twist range, or a hinge), and solve on a pose before it is applied:

```rust
use regia_animation::chain_ik::{ChainIkSettings, JointChain, JointLimit, solve_fabrik};
use regia_animation::{Pose, Skeleton};
use regia_core::math::Vec3;

fn reach_with_tail(skeleton: &Skeleton, pose: &mut Pose, target: Vec3) {
    let Ok(chain) = JointChain::between(skeleton, "tail_01", "tail_06") else {
        return; // not this rig
    };
    let chain = chain.with_uniform_limit(JointLimit::Cone { max_swing: 0.6, min_twist: -0.2, max_twist: 0.2 });
    let report = solve_fabrik(skeleton, pose, &chain, target, ChainIkSettings::default());
    // `report.error` is the remaining tip distance in meters (0 when reached).
    let _ = report.error;
}
```

`solve_ccd` has the same signature. FABRIK converges in fewer iterations on long chains; CCD
favours the joints near the tip (good for tails). Limits are relative to each joint's bind
rotation. Both reach reachable targets within a millimeter in the unit tests.

### Chain IK and body aim as graph nodes

The same solvers are graph nodes from `.animgraph.ron` version 4, so a tail, tentacle, spine, insect
leg or turret stack is authored in data and needs no Rust. `ChainIK` wraps an input pose and moves the
chain from `root` to `tip` onto a target; `BodyAim` turns a stack of joints, each by its share, until one
joint's axis points at a target:

```ron
ChainIK((
    inner: Clip((clip: "Idle", looping: true, speed: 1.0, sync_group: None)),
    root: "tail_01",
    tip: "tail_06",
    solver: Ccd,                       // or Fabrik (the default)
    target: Param("TailTarget"),       // a vec3 parameter, or Socket("name")
    pole: Some("TailPole"),            // optional: the side the chain bends towards
    limits: Some(Uniform(Cone(max_swing: 0.6, min_twist: -0.2, max_twist: 0.2))),
    iterations: 24,
    tolerance: 0.001,                  // meters
    weight_param: Some("TailWeight"),
))
```

```ron
BodyAim((
    inner: ...,
    joints: [("turret_yaw", 1.0), ("turret_pitch", 1.0)],  // bottom first, with shares
    aim_joint: "muzzle",
    axis: (0.0, 0.0, -1.0),            // the muzzle's forward
    target: Param("AimTarget"),
    limits: Some(PerJoint([(joint: "turret_pitch", limit: Hinge(axis: (1.0, 0.0, 0.0), min: -0.3, max: 1.1))])),
    passes: 8,
    weight_param: None,
))
```

`limits` is either one limit for every joint (`Uniform`) or a list by joint name (`PerJoint`); joints
not listed are free. Raise `passes` when the aim joint sits well away from the turning joints (a long
barrel offset from its pivot): each pass removes most of the miss the previous one left. In the editor,
add the nodes as **Chain IK** and **Body Aim** from the graph canvas's menu; the inspector picks joints
from the skeleton. `docs/formats/chain_ik.animgraph.ron` is a complete example. These two nodes (and a `PerJoint`
entry) refuse a field they do not know, so a misspelled `iteration:` is a load error instead of a silent
default, and every `BodyAim` joint needs a name.

## Full-body aim and hand placement

`regia_animation::aim_ik::solve_body_aim` turns a spine/neck/head chain (or a turret's yaw and
pitch joints) so an aim joint's axis points at a target, spreading the turn by per-joint shares
and limits. Hand grips re-attach the hands to the aimed frame, so a two-handed weapon stays in
both hands. `place_hand` reaches a hand to a position and orientation on its own (lean on a wall,
grab a ledge).

## Retargeting

`regia_animation::retarget` plays animation authored for one skeleton on another.
`RetargetProfile::guess_humanoid` maps the humanoid roles (hips, spine, arms, legs, …) from
common naming conventions; custom rigs (walkers, turrets) name their own roles with
`RetargetProfile::with`. The retargeter corrects for different rest poses (an A-pose rig drives
a T-pose rig) and scales the hips translation and root motion by hip height. Bake clips once at
load:

```rust
use regia_animation::retarget::Retargeter;
use regia_animation::{AnimationClip, Skeleton};

fn clips_for_new_rig(source: &Skeleton, target: &Skeleton, clips: &[AnimationClip]) -> Vec<AnimationClip> {
    match Retargeter::humanoid(source, target) {
        Ok(retargeter) => retargeter.retarget_library(clips),
        Err(error) => {
            eprintln!("cannot retarget: {error}");
            Vec::new()
        }
    }
}
```

### Keeping feet and hands planted

The retargeter keeps the target's bone lengths and scales only the hips. A target whose legs are
longer or shorter than that scale says then slides or sinks its planted feet, and two hands on one
prop drift apart. IK goals fix this at bake. Each goal names an `[upper, lower, end]` chain by roles.
After the pose transfer, the two-bone solver puts the chain's end where the source put it (times
the scale):

- `IkGoalMode::Contacts`, for feet. The foot holds still while the source foot is planted, and it
  stands at the target's own ankle height. The planted frames come from the clip's contact curves
  when it has them (see [Foot contact curves](#foot-contact-curves)).
- `IkGoalMode::ScaledWorld`, for hands on a prop or a weapon grip. The gap between the hands stays
  the source's gap times the scale.

```rust
use regia_animation::retarget::{IkGoalSpec, RetargetOptions, RetargetProfile, Retargeter};
use regia_animation::{AnimationClip, Skeleton};

fn planted_walks(source: &Skeleton, target: &Skeleton, clips: &[AnimationClip]) -> Vec<AnimationClip> {
    let options = RetargetOptions {
        ik_goals: IkGoalSpec::humanoid_feet(), // add IkGoalSpec::humanoid_hands() for a two-handed hold
        ..RetargetOptions::default()
    };
    let profile = |s: &Skeleton| RetargetProfile::guess_humanoid(s);
    match Retargeter::with_options(source, &profile(source), target, &profile(target), &options) {
        Ok(retargeter) => retargeter.retarget_library(clips),
        Err(error) => {
            eprintln!("cannot retarget: {error}");
            Vec::new()
        }
    }
}
```

A `.retarget.ron` setup saves the goals in its `ik_goals` list (format v2; see
`docs/formats/retarget-ron.md`). The editor's Retarget panel has a goals list with role pickers and
a toggle that previews the bake with its goals. `Retargeter::check` and `anim.retarget_check`
report each goal's residual and the planted slide. See
[Retargeting in the editor](../agents/retargeting.md).

## Root motion on characters

A clip with root motion (`RootMotionTrack`, extracted at import as XZ, XYZ or yaw) pins the rig
in place and reports the motion in `RootMotionOutput`. Add `RootMotionDriven` to a
`Character` and the framework moves the capsule by that motion through the character
controller — walls, steps and slopes still apply:

```rust
use regia_framework::character::{RootMotionApply, RootMotionDriven};
use regia_app::prelude::*;

fn drive_by_animation(mut commands: Commands, character: Entity, rig: Entity) {
    commands.entity(character).insert(RootMotionDriven {
        rig: Some(rig), // `None` when the character entity carries the Animator itself
        mode: RootMotionApply::Horizontal,
        rotation: true,
        scale: 1.0,
    });
}
```

`Horizontal` takes the animation's ground motion and yaw while walking and keeps gravity and
jumps physical; `Full` also takes vertical motion (climbs, vaults). The motion accumulates per
animation evaluation and is drained by the fixed step, so none is lost or doubled when frame
and fixed rates differ.

Graphs blend root motion the way they blend poses (ADR-871). A blend space halfway between walk
and run moves at the average speed, and a cross-fade from idle into walk ramps up with the fade
instead of jumping to full speed. A frame that covers several loops of a clip (a hitch, or a far
LOD animator updating rarely) counts every loop, and a negative speed plays the clip, its root
motion and its notifies backwards. `anim.clip_report` lists what one loop of each clip moves the
character and flags broken clip data; see
[Animation robustness](../agents/animation-robustness.md).

## Ragdolls and hit reactions

`regia_framework::ragdoll` puts capsule (or sphere or box) bodies on a rig (one per bone, ball-jointed, adjacent
bones not colliding). Add `RagdollPlugin` (with `PhysicsPlugin` and `AnimationPlugin`) and a
`Ragdoll` on the rig:

```rust
use std::sync::Arc;
use regia_animation::Skeleton;
use regia_framework::ragdoll::{Ragdoll, RagdollHit, RagdollMode, RagdollProfile};
use regia_app::prelude::*;
use regia_core::math::Vec3;

fn add_ragdoll(mut commands: Commands, rig: Entity, skeleton: Arc<Skeleton>) {
    let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 80.0));
    ragdoll.mode = RagdollMode::Powered;
    commands.entity(rig).insert(ragdoll);
}

fn shot_in_the_arm(mut hits: MessageWriter<RagdollHit>, rig: Entity) {
    hits.write(RagdollHit {
        rig,
        joint: "lowerarm_l".into(),
        impulse: Vec3::new(0.0, 0.0, -8.0),
        reaction: 1.0,
        recover: 0.8,
    });
}
```

- **Kinematic**: the bodies follow the animation (hit boxes).
- **Powered**: the pelvis follows the animation; the other bodies track it by velocity, so a hit
  knocks the struck limb off the pose and it recovers. Only the hit bone and its children show
  the simulation (a partial blend that decays over `recover`), plus `blend` everywhere.
- **Limp**: every body is free and the mesh blends fully to the simulation (death, knock-down).
- **Active** (spec 93 § 10): every body is free, and per-bone PD motors (`RagdollProfile::active.motors`,
  or the `.ragdoll.ron` v3 `active` block) pull each joint towards the animated pose. Feet with
  `balance: Ankle` motors keep it standing; a hit weakens the struck group (spine, arms, legs) for
  `hit.recover_time`; when it can no longer stop over its feet it falls, settles and reports a get-up
  clip (`RagdollBodies::active().get_up_clip()`). Play that clip and switch to `Kinematic`: the fallen
  pose blends into it. With no motor gains an active ragdoll is exactly limp. See
  [active ragdolls](../agents/ragdoll-active.md) and `crates/regia_framework/tests/ragdoll_active.rs`.

`crates/regia_framework/tests/ragdoll.rs` is the test scene: a humanoid falls limp onto a floor with
its joints intact, and a powered one takes a forearm hit and recovers. Agents read the ragdoll
mode and weight from `anim.get_state` ([agent guide](../agents/animation.md)).

## Authoring in the editor

The editor's **Anim Graph**, **Clip** and **Skeleton** tabs (bottom dock) author animation
without code. Double-click a `.animgraph.ron` in the Content Browser to open it.

- **Anim Graph** — the blend tree is a node canvas: right-click to add nodes, drag from a pin to
  connect, wire the final node into **Output**. Double-click a state machine's title (or "Open
  states") for its state canvas: right-click to add states and transitions, drag states, double-click a
  state to edit its sub-graph; breadcrumbs lead back. The left column declares parameters and
  inspects the selection (clip, blend-space points on a 1D/2D field — drag, double-click to add,
  right-click to remove — layers and joint masks, IK chains, transition conditions). The right
  column previews the graph on the rig with the parameter values you set. During Play-in-Editor
  the active nodes and state light up orange and the parameters show the game's values.
- **Clip** — scrub or play a clip on the rig; double-click the timeline to add a notify, drag
  markers to move them; add named curves or property tracks and shape them in the curve editor
  (right-click a key for constant / linear / cubic, drag tangent handles); pick the root-motion
  mode and see the root's path from above.
- **Skeleton** — the joint tree, sockets (add on the selected joint, offset, rename) and two-bone
  IK chains used by the graph's IK nodes.

Edits undo with Ctrl+Z like level edits. Saving writes the graph (`.animgraph.ron`, plus an
editor-only `.animgraph.layout.ron` with node positions) and, for the rig, a
`<model>.glb.animedit.ron` sidecar with the authored notifies, curves, sockets and chains
(`docs/formats/animedit-ron.md`). The agent surface is on the
[animation editor](../agents/animation-editor.md) agent page.

Legged units are authored in the Inspector and tried in the preview. A `GaitRig` and a
`FootPlacement` each get an authoring section below their fields: the settings with their
allowed ranges, leg chain pickers that list the rig's joints, gait pattern presets (tripod,
ripple, wave, quadruped walk, trot, pace, bound, biped walk), and for the gait a `.gait.ron`
picker that loads a file's settings into the rig or saves the rig's settings as one. Every
change is one undo step. "Preview in the test bed" copies the rig's gait into the Anim Graph
preview's **Locomotion test bed** section, which walks it at a set speed and turn rate over flat
ground, a slope, stairs, seeded rubble or a two-layer tunnel. Play or step it with the locomotion
gizmos drawn, and read the largest planted slide, the largest foot jump, the forced steps and the
stability margin. Agents run the same walk with `editor.anim_locomotion_preview`.

## Spring bones

Hair, tails, antennae and cloth strips can move on their own after the animation: add `SpringBones` with a
`.spring.ron` setup (chains of joints, stiffness, damping, gravity, body colliders). See
[Spring bones](../agents/spring-bones.md) and `docs/formats/spring-ron.md`.

## Foot contact curves

Feet stay planted best when the clip itself says when each foot is on the ground. Add
`contact_curves: Some(())` to a character file's `.meta` settings and the cook writes a curve
`contact.<foot joint>` onto every clip: 1 while the foot is planted, 0 while it swings, worked out from
how low and how slow the foot is in that clip. The feet are found from the joint names; name them
yourself with `contact_curves: Some((feet: ["foot_l", "foot_r"]))`, and add `treadmill: true` for
clips that walk in place.

The curves blend with the clips (half a walk and half a run gives a half-planted foot where only one of
them plants) and the foot planter can use them instead of guessing from the foot's height, which
matters for clips whose feet are not at the character's origin, for retargeted clips and for blends.
See [foot planting](../agents/foot-planting.md); `anim.clip_report` shows the curves a clip carries.

## Stride and orientation warping in graphs

A locomotion blend space is authored at a few speeds and directions, but gameplay moves the character at
any speed and in any direction. When the character moves faster than the clips, the planted feet slide
backward over the ground ("skating"); when it strafes at an angle the blend space has no clip for, the legs
point the wrong way. Two graph nodes (`.animgraph.ron` version 4) fix this with the same code the motion
matching player uses.

`StrideWarp` compares the character's real ground speed (a float parameter you set every frame, in m/s)
with the speed of the clips under it (their root motion this frame), and either plays them faster or slower
(`mode: Rate`) or makes the steps longer or shorter with leg IK (`mode: Pose`, the default):

```ron
StrideWarp((
    inner: BlendSpace1D((...)),              // your walk/run blend, with root motion
    ground_speed_param: "GroundSpeed",
    settings: (
        strength: 1.0,
        min_speed: 0.1,                      // m/s; slower than this, nothing is warped
        ratio_min: 0.8,                      // the largest slow-down
        ratio_max: 1.4,                      // the largest speed-up
        response: 12.0,                      // how fast the ratio follows, per second
        mode: Pose,                          // or Rate
    ),
    legs: [("thigh_l", "calf_l", "foot_l"), ("thigh_r", "calf_r", "foot_r")],
    pelvis: "pelvis",
    stride_length: 0.5,                      // meters
))
```

`Rate` needs no legs: it speeds the whole input up or down, and blend-space samples and sync groups under it
stay in step (keep every clip of a sync group under the same warp node). `Pose` keeps the timing and stretches the stride, lowering the pelvis a little when a leg would
otherwise be too short. Leave `settings` out for full strength in `Pose` mode. Keep `ratio_max` above the
largest speed-up you expect: past it the feet skate again.

`OrientationWarp` turns the hips toward the direction the character is really moving and turns the spine
back, so the chest keeps facing where the character looks:

```ron
OrientationWarp((
    inner: StrideWarp((...)),
    heading_param: "MoveHeading",            // radians: 0 = forward, positive = toward the left
    pelvis: "pelvis",
    spine: [("spine_01", 0.5), ("spine_02", 0.5)],   // shares of the counter-turn, summing to 1
    strength: 1.0,
    max_delta: 1.0,                          // the largest turn, radians
))
```

Set `MoveHeading` from the movement direction in the character's own space;
`regia_animation::warp_nodes::movement_heading(direction)` does the conversion. Both nodes need clips with
root motion: without it they leave the pose alone and log `animgraph_warp_no_root_motion` once. The editor
lists them as **Stride Warp** and **Orientation Warp**, and `anim.get_state` shows each node's current ratio
and turn under `warps`. Distance matching (playing a stop clip by the distance left) is still only available
in motion matching. `docs/formats/locomotion_warp.animgraph.ron` is a complete example.

## Foot placement with FootPlacement

Add a `FootPlacement` component to a rig and the engine plants its feet. Name each leg's hip, knee
and ankle (an optional toe joint is used by heel and toe placement, below). The scene needs one `GroundProbes`
resource: the character plugin installs a physics raycast when you have not supplied one. Tests
can install an analytic plane.

Pose output (`FootPlacementOutput::Pose`, the default) poses the skeleton after the ragdoll blend
and before eye gaze. Graph output names the parameters a `TwoBoneIK` node already reads
(`Foot{i}Target`, `Foot{i}Weight`, `Foot{i}Pole`) and writes them before the graph evaluates. A
graph plants the tip joint on the contact. Pose output leaves the ankle a bind height above the
ground so the sole meets it. Place the rig each frame without last frame's pelvis; graph output
adds this frame's pelvis.

`grounded` set to `Some(false)` fades the feet back to the clip (in the air). A curve or float
parameter named `FootPlacementWeight` does the same: 0 fades out at the planter's blend-out rate,
and any positive value fades back in. A vault or climb montage can put that curve at 0. A jump of
the rig farther than `teleport_distance` (2 meters) clears the pins, and so does a respawn.

`FootPlantReport` (including `max_slide`) and `FootPlacementStatus` are on the rig afterwards.
See [Foot planting](../agents/foot-planting.md).

## Legged units: sim body, presentation feet

A legged unit has two halves. The **body** (position, heading, velocity, collision, which slopes it
can climb) belongs to your game's simulation or mover: gameplay reads it. The **feet** (placement,
swing arcs, body height, leg IK, foot tilt, pelvis) are presentation: nothing in gameplay reads
them. Footsteps that matter to gameplay (stomp damage, noise that AI hears) come from the
simulation's own cadence, such as distance travelled over step length, never from the visual feet.

For the six-leg stepper, drive the feet from your body each frame:

```rust
use regia_animation::gait::{GaitBody, GaitError, GaitParams, GaitState, step_gait_driven};

/// Called once per presentation frame with the body your simulation computed this frame.
fn place_feet(
    params: &GaitParams,
    gait: &mut GaitState,
    sim_body: GaitBody,
    dt: f32,
    ground_height: impl Fn(f32, f32) -> f32,
) -> Result<(), GaitError> {
    step_gait_driven(params, gait, sim_body, dt, ground_height)?;
    // gait.feet[i].position are the targets for your leg IK; gait.position[1] is the body height.
    Ok(())
}
```

`GaitBody` uses Regia's object convention: +Y up, +X right, -Z forward, yaw about +Y. Create the
state with `GaitState::at_rest_driven`. If your simulation teleports the unit or shifts its origin
by more than one step length, the feet replant under the new body instead of stretching, and
`GaitState::replants` counts it. The older `step_gait` moves the body itself from stick controls
(+Z forward) and is meant for previews and tests. See [Six-leg tripod gait](../agents/tripod-gait.md).

## Legged units: stopping and turning in place

When a legged unit stops or turns on the spot (ground speed at or below `idle_speed`), its feet
take steps instead of snapping under the body. The step in progress finishes on its arc, then
each tripod with a foot out of place steps to its neutral spot under the hip, one tripod at a
time. While the unit turns in place, a foot steps once it has fallen far enough behind the
rotation, and the other tripod stays on the ground meanwhile. Four settings tune this:

| setting | meaning | try |
|---|---|---|
| `settle_distance` | how far (meters) a foot may rest from its neutral spot before it steps after a stop | about a tenth of `step_length` |
| `settle_period` | how long (seconds) each settling or turning step takes | close to the walking period at a slow walk |
| `turn_step_distance` | how far a planted foot may fall behind while turning in place before its tripod steps | a quarter of `step_length` |
| `turn_step_angle` | the same as an angle (radians) around the body center; catches feet close to the body | 0.3 to 0.4 |

`GaitState::settled` is true once the feet are at rest; a settled foot only follows the floor
height. Agents can wait for it after a stop (see [Six-leg tripod gait](../agents/tripod-gait.md),
"Stopping and turning in place").

## Feet on stairs: heel and toe

By default `FootPlacement` probes the ground under each ankle only. On stairs, curbs and steep
ramps set `foot_shape` to `Some(FootShapeSettings::default())` and name each leg's toe joint. The
engine then probes the heel and the toe tip as well. The foot pitches to match a ramp, rests flat
on a stair tread instead of sinking its toe into the riser, and moves onto the tread it mostly
covers when only a sliver of it is over the step (at most 12 cm by default). While the clip lifts
the heel at the end of a step, the toe joint bends so the toes stay on the ground. `heel_gap` and
`toe_gap` in `FootPlantReport` tell how far each planted heel and toe tip sits over the tread.
See [Foot planting](../agents/foot-planting.md#heel-and-toe-stairs-and-toe-bend).

## Legged units: ground, holes and cliff edges

The six-leg stepper asks your game where a foot can stand through `GaitGround`. A plain height
closure still works, but a small adapter lets the stepper handle tunnels, bridges, holes and
cliff edges:

<!-- ignore-reason: adapter sketch over the host's own world type (`MyWorld`, `cast_down`) -->
```rust,ignore
use regia_animation::gait::{GaitGround, GroundHit};

/// Your world's floors, seen from a walker: the layer near `expected_y`, or nothing.
struct Floors<'a> {
    world: &'a MyWorld,
}

impl GaitGround for Floors<'_> {
    fn sample(&self, x: f32, z: f32, expected_y: f32, band: f32) -> Option<GroundHit> {
        // For example a downward ray from `expected_y + band` that stops `2 * band` lower.
        let hit = self.world.cast_down(x, expected_y + band, z, 2.0 * band)?;
        Some(GroundHit { y: hit.y, normal: hit.normal })
    }
}
```

`expected_y` is where the walker's floor should be (its body height minus `clearance`), so a unit
in a tunnel finds the tunnel floor, not the hill above it. Return `None` where there is no ground.
Five settings tune the behavior:

| setting | meaning | try |
|---|---|---|
| `ground_band` | how far (meters) above or below the expected floor a foothold may be | a little more than the tallest step the unit climbs |
| `foothold_candidates` | how many nearby spots to try when a landing is refused (8 per ring, at most 64) | 16 |
| `foothold_spacing` | distance (meters) between the search rings | a little more than the foot's width (0.25 for a 0.6 m stride) |
| `max_reach` | how far (meters) each foot may be from its hip before it steps on its own | the leg's length, at least `sqrt(step_length^2 + clearance^2)` |
| `hang_drop` | how far (meters) a foot with no ground dangles below the expected floor | a third of the leg |

A landing in a hole, on a slope steeper than `max_slope` or past the edge moves to the nearest good
spot around it. A foot that falls too far behind takes its own step (`GaitState::forced_steps`
counts these). A foot with nowhere to stand hangs instead of failing (`planted` is false). Each
foot's `normal` tilts your foot IK to the ground. See [Six-leg tripod gait](../agents/tripod-gait.md),
"Ground: layers, holes, cliff edges and reach".

## Legged units: any number of legs

The procedural stepper drives 1 to 16 legs with any gait (ADR-8653). Pick a pattern and give one hip per
leg:

<!-- ignore-reason: fragment of a host's setup; `my_params` stands for the host's own GaitParams -->
```rust,ignore
use regia_animation::gait::{GaitParams, GaitPattern, GaitPatternSet, MAX_LEGS, leg_array};

let hips = leg_array([[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]], [0.0; 2]);
// Walk below 1 m/s, trot up to 2 m/s, bound above; 0.1 m/s of hysteresis around each switch.
let patterns = GaitPatternSet::new(
    &[GaitPattern::quadruped_walk(), GaitPattern::trot(), GaitPattern::bound()],
    &[1.0, 2.0],
    0.1,
)?;
let params = GaitParams { hip_offsets: hips, max_reach: [1.0; MAX_LEGS], patterns, ..my_params };
```

| creature | pattern |
|---|---|
| six-legged insect or mech | `GaitPattern::tripod()` (fast), `ripple(6)`, `wave(6)` (slow, steady) |
| spider, centipede (8 to 16 legs) | `wave(n)` or `ripple(n)` |
| four-legged animal or walker | `quadruped_walk()`, `trot()`, `pace()`, `bound()` |
| two-legged robot | `biped_walk()` |

Legs are ordered in pairs from the front, left before right. `GaitPattern::new(&offsets, duty)` makes any
other gait: each leg's offset in the stride cycle and the fraction of the cycle a foot stays down. A switch
between patterns blends over one stride, so no foot jumps. `GaitState::stability_margin` tells you how far
the body center is inside the planted feet (negative: the pose would tip over standing still, which is
normal for a trot or bound). See [Procedural gait](../agents/procedural-gait.md), "Gait patterns: any leg
count and gait".

## Legged units: the engine walks the rig

To have the engine walk a legged rig, with no stepping, axis conversion or leg IK in your code, add a
`GaitRig` (ADR-8707). Name each leg's first and last joint in pattern order:

<!-- ignore-reason: fragment of a host's setup; `params` stands for the host's own GaitParams -->
```rust,ignore
use regia_animation::gait_rig::{GaitLeg, GaitRig};

let legs = (0..6).map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}"))).collect();
commands.entity(rig).insert(GaitRig::new(params, legs).with_body("body"));
```

Move the rig entity as usual (or set `mode: GaitRigMode::Integrating` and write a `GaitControls`
component). Each frame the engine steps the gait, finds the ground through the same `GroundProbes`
resource that foot placement uses, raises the body joint to the stance height, and bends every leg so
its foot lands on its target. A leg with three joints uses two-bone IK; a longer leg uses FABRIK. The
neutral stance comes from the rig's bind pose, so you do not type hip offsets. A misspelled joint is
reported in `GaitRigStatus` (`gait_rig_leg_unresolved`) and that leg is skipped. See
[Procedural gait](../agents/procedural-gait.md), "GaitRig: the engine walks the rig".

## Legged units: body height and tilt on rough ground

By default the stepper's body sits at the average height of the planted feet and stays level, so on
rubble it hops each time a new group of feet lands, and on a ramp it stays flat while its legs
stretch. Set `GaitParams::posture` to let the body follow the ground (ADR-8706):

<!-- ignore-reason: fragment of a host's setup; `my_params` stands for the host's own GaitParams -->
```rust,ignore
use regia_animation::gait::{GaitParams, GaitPosture};

let params = GaitParams {
    posture: GaitPosture {
        swing_weight: 0.5,          // feet in the air count half in the height target
        height_tau: 0.08,           // seconds to ease toward the target
        height_slew: 0.6,           // at most 0.6 m of height change per second
        attitude_weight: 1.0,       // take the whole ground tilt (0 = stay upright)
        max_pitch: 25f32.to_radians(),
        max_roll: 20f32.to_radians(),
        attitude_tau: 0.1,          // seconds to ease the tilt
    },
    ..my_params
};
```

| setting | meaning | try |
|---|---|---|
| `swing_weight` | how much a foot in the air counts toward the body height (it counts at the ground under it) | 0.5; 1 for the smoothest height |
| `height_tau` | how quickly the body height settles, in seconds | about a quarter of a step |
| `height_slew` | the fastest the body may rise or sink, in meters per second | a little more than your steepest climb rate |
| `attitude_weight` | how much of the ground's tilt the body takes | 1 for walkers, 0.5 for a heavy hull that leans less |
| `max_pitch`, `max_roll` | the largest tilt, in radians | 20 to 30 degrees |
| `attitude_tau` | how quickly the tilt settles, in seconds | 0.1 |

Each step `GaitState::attitude` holds the body's tilt as a quaternion `[x, y, z, w]` in the walker's
own frame; multiply your yaw rotation by it to place the body mesh. `pitch` and `roll` give the same
tilt in radians (positive pitch raises the nose), and `body_height_target` is where the height is
heading. On level ground the tilt is exactly none. `GaitPosture::UPRIGHT` keeps the old level body.
See [Procedural gait](../agents/procedural-gait.md), "Posture: body height and tilt".

## Checking locomotion quality

Four agent methods judge procedural locomotion without looking at a screen (ADR-8708):

- `anim.locomotion_state` reads one character's gait and foot planting in one answer: which feet are
  planted, the ground normals, how many footholds were refused, the stability margin and the foot
  planter's slide and tilt.
- `anim.gait_simulate` runs the procedural stepper with your settings over a flat, sloped, stepped or
  seeded rubble ground and reports how far any foot moved in one step (`max_foot_jump`, compared with
  `swing_bound`), whether a planted foot ever slid (`max_planted_slip`), how many feet stayed down and how
  far the legs reached. The same call always gives the same answer, so it works as a regression check
  for gait settings.
- `anim.foot_plant_simulate` walks the built-in demo character with your `FootPlantSettings` and reports
  foot slide, sinking, floating and tilt.
- `anim.ik_report` tells how close each IK node of a character's animation graph got to its target.
  Turn it on with `enable: true`, play a frame and read it; while it is off it costs nothing.

In Rust the same IK numbers come from `Animator::set_ik_debug(true)` and `Animator::debug_snapshot()`
(its `ik` rows). See [Locomotion quality](../agents/locomotion-quality.md).

## Legged units: settings in a `.gait.ron` file

A walker's settings do not have to be Rust code. Put them in a `<name>.gait.ron` file (format
[docs/formats/gait-ron.md](../../docs/formats/gait-ron.md)): the patterns and the speeds that switch
between them, the speed limits, step length, lift and clearance, slope and reach limits, settle and turn
thresholds, the foothold search, the ground band, the body tilt limits, and the legs by joint name.

<!-- ignore-reason: fragment of a host's setup; `commands` and `rig` are the host's own -->
```rust,ignore
use regia_animation::gait_asset::GaitAssets;
use regia_animation::gait_rig::GaitRig;

fn spawn_walker(mut commands: Commands, mut gaits: ResMut<GaitAssets>, rig: Entity) {
    match gaits.load_file("assets/units/walker.gait.ron") {
        Ok(handle) => {
            if let Some(asset) = gaits.get(&handle) {
                commands.entity(rig).insert(GaitRig::from_asset(handle.clone(), asset));
            }
        }
        Err(error) => tracing::warn!(code = error.code(), "walker settings refused: {error}"),
    }
}
```

Save the file while the game runs and the walker takes the new settings on the next frame. A save the
engine refuses (a negative step length, a pattern for four legs on a six-leg rig) keeps the walker on the
settings it had, and `GaitAssets::error` tells you why, with the same code `anim.gait_validate` returns
(`gait_settings`, `gait_pattern`, `gait_legs`, `gait_parse`, `gait_version`). Leave `hip_offset` out of a
leg to have it measured from the rig's bind pose; give it to place that foot yourself. See
[Procedural gait](../agents/procedural-gait.md), "Gait files: `.gait.ron`".

## Crowds of walkers: IK level of detail

Foot placement and the gait rig cost ground probes and leg solves every frame. For a crowd, let far
characters do less: set the `AnimLodCamera` resource to the camera position each frame, and every
`FootPlacement` and `GaitRig` picks a level by distance (ADR-8728):

- within `lod.full_within` (default 30 m): full detail, as before;
- up to `lod.reduced_within` (default 80 m): the ground is probed every `lod.reduced_interval` frames
  (default 4) and the last hits are reused in between; feet are still solved every frame;
- beyond: off. Feet fade back to the animation in about a tenth of a second and are then left alone;
  a gait rig that you move yourself stops stepping and restarts cleanly when it comes back.

Without an `AnimLodCamera` everything stays at full detail. `lod.force` sets a level by hand (for a
close-up or your own budget logic). The chosen level is shown in `FootPlacementStatus` and
`GaitRigStatus` (`lod`). `cargo run --release -p regia_animation --example locomotion_cost -- --scaling`
prints the cost per frame on your machine. See [Foot planting](../agents/foot-planting.md), "IK level
of detail".

## Legged units: see what the feet decided

To tune a walker or a foot planter, turn on the `locomotion` debug overlay. In code, call
`set_enabled("locomotion", true)` on the app's `GizmoConfigStore` resource, or ask the agent with `debug.set_gizmos { "group": "locomotion", "enabled": true }`. Each frame it
draws, for every `GaitRig`: the foot targets (planted and swinging in different colors), the hips,
each swing arc from lift-off to landing, the support polygon of the planted feet, the stability margin
as a ring around the body, markers on refused and forced steps, and the body's attitude frame. For every
`FootPlacement` rig it draws the probe rays and hits with the ground normal, the pins with a bar as tall
as their lock weight, the tilted soles, the pelvis offset and a ring as wide as each foot's slide. The
leg IK of both shows its joints and bones, its target, the knee pole, the reach sphere and any joint
limits. The three parts switch separately (`gait`, `foot_placement`, `ik`; see
[Gizmos](../agents/gizmos.md), "Sub-toggles"). The colors come from the theme's tokens
(`color.success` for planted feet, `color.warning` for swinging ones, and so on); replace the
`LocomotionGizmoColors` resource to change them. The overlay is off by default and costs nothing
while it is off.

## Legged units: a worked example

`samples/anim_lab` has a locomotion scene you can copy (`samples/anim_lab/src/locomotion.rs`). It needs
no downloaded content and no renderer: a six-legged walker with a `GaitRig` walks over three low steps and
a 10° slope, stops and turns in place, and a two-legged character with a `FootPlacement` walks up a flight
of stairs again and again. Both are moved by a small script that stands in for your game's mover: it sets each
rig's `Transform` every frame, and a `GaitVelocity` for the walker. The engine does the rest.

The ground is one `GroundProbe`, installed once for the whole scene. Both the gait rig and foot placement
cast straight down through it:

<!-- ignore-reason: excerpt of samples/anim_lab/src/locomotion.rs (Course::height and Course::normal live there) -->
```rust,ignore
use regia_animation::foot_ik::{GroundHit, GroundProbe, GroundProbes};

struct Course;

impl GroundProbe for Course {
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let y = Course::height(from.x, from.z);
        let depth = from.y - y;
        (down == Vec3::NEG_Y && (0.0..=max_distance).contains(&depth)).then(|| GroundHit {
            point: Vec3::new(from.x, y, from.z),
            normal: Course::normal(from.x, from.z),
        })
    }
}

app.insert_resource(GroundProbes::new(Course));
```

The walker is a skeleton, an `Animator` (any clip; the gait rig poses the legs after it), a `GaitRig`
and the velocity your mover reports:

<!-- ignore-reason: excerpt of samples/anim_lab/src/locomotion.rs (add_scene) -->
```rust,ignore
let legs = (0..6).map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}"))).collect();
let binding = spawn_skeleton(app.world_mut(), Arc::new(hexapod_skeleton()?));
app.world_mut().spawn((
    Transform::from_translation(start),
    binding,
    Animator::single(AnimationClip::empty("still", 1.0), true),
    GaitRig::new(hexapod_params(), legs).with_body("body"),
    GaitVelocity::default(), // your mover writes `linear` each frame
));
```

The character plays its walk clip in place and gets a `FootPlacement` that names both legs and their
toe joints, with heel and toe probes on:

<!-- ignore-reason: excerpt of samples/anim_lab/src/locomotion.rs (biped_placement) -->
```rust,ignore
let mut placement = FootPlacement::default();
placement.legs = ["l", "r"]
    .map(|side| FootLegDesc {
        hip: format!("thigh_{side}"),
        knee: format!("calf_{side}"),
        ankle: format!("foot_{side}"),
        toe: Some(format!("ball_{side}")),
        pole_offset: Vec3::ZERO,
    })
    .to_vec();
placement.foot_shape = Some(FootShapeSettings::default());
```

`cargo test -p anim_lab --test locomotion_acceptance` runs the scene and checks it with numbers: no planted
foot slides, no foot jumps in a single frame, the walker never needs a forced step, and the character's
heels and toes never sink into the treads. `cargo run -p anim_lab` prints the same numbers on its `locomotion`
line (the binary also downloads two sample models for its other cases). `tests/locomotion_agent.rs` shows an agent checking the scene with `scene.query_entities`,
`play.step`, `play.expect` and `anim.locomotion_state` (see
[Locomotion quality](../agents/locomotion-quality.md)).
