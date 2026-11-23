# Foot planting: tilt, plant lock and smoothing

`regia_animation::foot_ik::FootPlanter` keeps a character's feet on the ground as it walks over
slopes, steps and rough terrain (ADR-8088). It is the stateful form of `plan_foot_ik`: keep one per
character and call `plan` once per frame. It adds three things the stateless planner cannot do:

- **Plant lock.** A planted foot is pinned in world space where it touched down and held until the
  clip lifts it, so a clip that does not match the character's speed or turn no longer slides over
  the ground. A pin left farther than `lock_max_drift` behind (a teleport, a sharp turn) is set again
  where the foot is; a lifted foot is handed back to the clip over `lock_release_time`. A stance
  starts when the clip foot is down (within `plant_lift`) and slow (under `lock_max_speed`) and ends
  as soon as the clip lifts it or it speeds up past 1.5 times `lock_max_speed`, at any frame rate
  (a re-pin never keeps a fast foot down). When the ground sample moves with the rig (a platform
  carries the whole body), the pin moves with it (ADR-8721). During the release the host probes
  under the clip foot; until the foot is 5 cm from where it lifted it never stands below the tread
  it lifted from, so stepping down it neither sinks into that tread nor floats over the lower one
  once it is past the edge (ADR-8801).
- **Foot tilt.** A planted foot turns toward the ground normal, up to `max_tilt` (25 degrees by
  default), and the ankle is lifted so the tilted sole still touches the ground.
- **Smoothing.** The ground under each foot has a dead band, an ease and a slew limit, so a probe
  that jumps (a stair edge) never pops the leg; the pelvis eases; the whole effect fades out in the
  air and back in on landing.

The planter is plain data: the host probes the ground, calls the planter and poses the skeleton.
It never touches the ECS. What it did is published for agents as the reflected component
`FootPlantReport`. A rig with `FootPlacement` receives that report from the engine each frame
(see [FootPlacement component](#footplacement-component)). Hosts that still call `FootPlanter`
by hand insert the report themselves.

## Drive it from a host

<!-- ignore-reason: a per-frame sketch over the game's own rig values (hip and ankle positions, probe hits, skeleton, pose, legs); the compiled end-to-end use is crates/regia_animation/tests/foot_plant.rs -->
```rust,ignore
use regia_animation::foot_ik::contact::contact_weight;
use regia_animation::foot_ik::{FootGround, FootPlantInput, FootPlantLeg, FootPlanter, apply_foot_plant};

// Once per character.
let mut planter = FootPlanter::new(Default::default());

// Every frame, per leg: the animated hip and ankle in world space, the leg length, the bind-pose
// ankle height over the sole, and a ground probe. While the stance holds a pin, probe under the pin.
let probe_at = planter.pin(leg).unwrap_or(ankle_world);
let leg_input = FootPlantLeg {
    hip: hip_world,
    foot: ankle_world,
    length: thigh + shin,
    ankle_height,
    ground: FootGround::Contact(hit_point),
    normal: hit_normal,
    contact: contact_weight(&animator.parameters, ankle_joint_name), // None: the lift rule
};
let plan = planter.plan(&FootPlantInput {
    rig_translation, rig_rotation, legs: &legs, active: grounded, dt,
});
apply_foot_plant(&skeleton, &mut pose, &plan, &ik_legs, Some(forward)); // or plan.ik_plan() for TwoBoneIK nodes
commands.entity(rig).insert(planter.report(&plan)); // omit this when the rig has FootPlacement
```

`active` is false in the air, prone, sliding, vaulting, ragdolled or dead. `apply_foot_plant` moves the
root joint by the pelvis offset; `FootPlant::ik_plan` instead returns targets for a rig entity moved by
the offset, the shape `plan_foot_ik` returns, for graphs whose `TwoBoneIK` nodes read target
parameters (the tilt is not part of that form).

Settings (`FootPlantSettings`, all with defaults): `max_ground_offset` 0.6 m, `max_pelvis_drop`
0.35 m, `max_tilt` 0.436 rad, `plant_lift` 0.03 m and `swing_lift` 0.08 m (the clip foot is planted
below the first, swinging above the second), `max_extension` 0.995, `blend_in_per_s` 5,
`blend_out_per_s` 10, `offset_tau` 0.05 s, `pelvis_tau` 0.07 s, `tilt_tau` 0.08 s, `deadband` 0.004 m,
`max_slew` 3.5 m/s, `lock` true, `lock_max_drift` 0.3 m, `lock_release_time` 0.15 s, `lock_max_speed`
1 m/s (a clip foot moving faster than this over the ground is swinging even when it is low, so it is not
pinned; a stance ends above 1.5 times this). A NaN or
negative value falls back to a safe one; the planter never panics on user data.

## Read and assert it as an agent

`FootPlantReport` is a reflected component, so the generic surface reads it:
`scene.get_entity` for its values and `play.expect` to assert one after `play.step`.

| Field | Meaning |
|---|---|
| `weight` | Effect weight 0 to 1 (0 in the air). |
| `pelvis_offset` | Pelvis offset, object space, meters (zero or negative). |
| `planted` | Per leg: 1 planted, 0 swinging; 0 while the effect is faded out (weight 0). |
| `lock` | Per leg: weight of the world-space pin, 0 to 1. |
| `tilt_deg` | Per leg: foot tilt toward the ground normal, degrees. |
| `slide` | Per leg: how far the leg's world target moved since its stance began, meters. |
| `max_slide` | The largest `slide` this frame. With the lock on it stays under 0.01. |

A check that a walking character does not skate: step the game while it walks and assert
`max_slide` stays at or below 0.01 every frame.

```json
play.expect { "assertions": [ { "condition": { "entity": 4294967310, "component": "FootPlantReport",
              "path": "max_slide", "op": "lte", "value": 0.01 } } ] }
```

On a slope, `tilt_deg` of a planted leg is the slope angle up to `max_tilt` (25 degrees by default).
In the air `weight` falls to 0 within 0.1 s and every `lock` is 0.

The engine-side acceptance is `cargo test -p regia_animation --test foot_plant`: the demo walk played
as an in-place clip while the character moves 15 % faster (the clip skates about 10 cm per stance)
keeps every planted ankle within 1 cm with the lock, on flat ground and on a 15 degree slope, for any
rig position and yaw (`planted_foot_never_slides_over_1cm`).

## Contact curves and the one lock (ADR-8308)

A stance can come from the clip's cooked contact curves instead of the foot's height. With
`contact_curves: Some(())` (or `Some((feet: ["foot_l", "foot_r"]))`) in a glTF source's `.meta`
settings, the cook labels every foot of every skinned clip with the motion-matching contact labels
(height over the foot's own lowest point in the clip and speed, with hysteresis) and stores them as
float curves `contact.<foot joint>`, 1 planted and 0 swinging. Set `treadmill: true` in the same block
for clips that walk in place. The graph blends these curves like any named curve (a 50/50 blend-space
mix of a planted and a swinging foot reads 0.5) and writes them into the animator's parameters;
`foot_ik::contact::contact_weight(&parameters, "<ankle joint>")` reads one back for
`FootPlantLeg::contact` (a weight from 0 to 1; `None` falls back to the lift rule). A stance needs the
weight at 1 to start and ends below 0.9.

The curve is what to use when the clip's sole is not at the rig origin, for retargeted clips and for
blend-space mixes: the lift rule measures the clip foot against the bind ankle height, which those
clips do not keep. `cargo test -p regia_animation --test contact_curves` shows it on the demo walk
lifted 5 cm: the lift rule never pins a foot, the curve pins every stance and `max_slide` stays at or
below 0.01 (`contact_curve_beats_the_height_rule`).

The planter's pin, teleport re-pin and release are `foot_ik::lock::PlantLock`, the same state machine
the motion-matching `FootLocker` and the crowd's exact-tier lock use, so a fix to the lock lands in all
three. Each keeps its own stance rule and drift measure; the outputs of all three are unchanged
(`cargo test -p regia_animation --test plant_lock_identity`).

An agent sees the curves a clip carries in `anim.clip_report`: each clip's `contact_curves` lists
`{ name, joint, keys, planted_share, touchdowns }` (see [animation](animation.md)).

## FootPlacement component

`FootPlacement` (ADR-8388) is the planter as a component. The engine probes, plans, applies and
inserts `FootPlantReport` every frame. `play.expect` on `max_slide` needs no host code that builds
the report: put the component on the rig and step.

```rust,wrap
# use regia_app::prelude::World;
use regia_animation::foot_ik::{FootLegDesc, FootPlacement, FootPlacementOutput, GroundProbes};
use regia_core::math::Vec3;
# let mut world = World::new();
# let rig = world.spawn_empty().id();
# let mut commands = world.commands();

let mut feet = FootPlacement::default();
feet.legs = vec![FootLegDesc {
    hip: "thigh_l".into(),
    knee: "calf_l".into(),
    ankle: "foot_l".into(),
    toe: None, // a toe joint name for heel and toe placement (see below)
    pole_offset: Vec3::ZERO,
}];
feet.probe_height = 0.6; // meters above the foot, or above the pin while the stance holds it
feet.probe_length = 1.2;
// Pose writes the skeleton inside apply_animation (after ragdoll, before eye gaze).
// GraphParams names the TwoBoneIK parameters (`{i}` is the leg index) and runs in Update
// before evaluate_animators, because a missing weight float is 0 and the node would do nothing.
feet.output = FootPlacementOutput::Pose;
// feet.output = FootPlacementOutput::GraphParams {
//     target: "Foot{i}Target".into(),
//     weight: "Foot{i}Weight".into(),
//     pole: "Foot{i}Pole".into(),
// };
commands.entity(rig).insert(feet);
```

The host places the rig without last frame's pelvis first. Graph output then adds this frame's
pelvis to the rig translation. Pose output puts the pelvis on the root joint and does not move
the entity.

Graph parameters aim the tip joint at the contact. The planter's target sits higher by the bind
tip height, which pose output needs so the sole meets the ground. A graph node moves the tip
itself, so that height is removed before the parameter is written. At placement weight 0 the
clip pose is unchanged.

`grounded`: `None` means always active. `Some(false)` fades the effect out. When the framework
character plugin is present it copies the character's grounded flag (`placement.character`, or
the rig itself). A montage or clip curve named `weight_param` (default `FootPlacementWeight`)
wins over the float parameter of that name. Absent means goal 1. A goal at or below 0 fades out
at `blend_out_per_s` (default 10); any positive goal fades back in at `blend_in_per_s` (default
5). After the fade the clip pose is unchanged, so a vault or climb montage can zero the curve.

A rig translation jump farther than `teleport_distance` (default 2 meters), or the one-frame
marker `FootPlacementRespawn` (the character plugin inserts it when `Died` is removed), clears
the pins. The same frame plans again from weight 0.

### GroundProbe

`regia_animation` does not depend on physics. Install one probe for the scene:

```rust,wrap
# use regia_animation::foot_ik::{GroundHit, GroundProbe, GroundProbes};
# use regia_app::prelude::*;
# use regia_core::math::Vec3;
# struct Plane;
# impl GroundProbe for Plane {
#     fn probe(&self, from: Vec3, _down: Vec3, _max: f32, _ignore: Entity) -> Option<GroundHit> {
#         Some(GroundHit { point: Vec3::new(from.x, 0.0, from.z), normal: Vec3::Y })
#     }
# }
# let mut app = App::new();
# let my_probe = Plane;
app.insert_resource(GroundProbes::new(my_probe));
```

`GroundProbe::probe(from, down, max_distance, ignore) -> Option<GroundHit { point, normal }>`.
`GroundProbes::probe` keeps only ground: a hit at the ray origin (a cast that started inside solid
geometry), above it, a back face (normal along the ray) or a non-finite hit reads as a miss, and a
miss is the flat floor at the rig origin (ADR-8721). A physics ray that starts inside a shape
reports the face it leaves by with that face's outward normal (along the ray), so a probe buried in
a slab reads as a miss rather than as ground on the slab's underside (ADR-8801). `GroundProbe::probe_moving` (a default
method: still ground) adds the ground's velocity at the hit; the physics probe reports the hit
body's velocity there (`PhysicsQuery::point_velocity`), and `FootPlacement` carries a planted foot
by it each frame (`FootPlanter::carry`), so a moving platform carries the feet with the body.
`ignore` is the character body, or the rig when `character` is unset. The framework's character
plugin installs a physics raycast (sensors and that body excluded) when no `GroundProbes`
resource is present. A test can install an analytic plane instead. The character plugin does not
replace a probe the host already inserted.

### FootPlacementStatus

Reflected, on the rig, updated every frame the component runs:

| Field | Meaning |
|---|---|
| `probe_installed` | A `GroundProbes` resource is on the world. A ray that misses still counts. |
| `legs_resolved` | Every hip, knee and ankle name resolved. A missing toe does not fail this. |
| `lod` | The IK level of detail this frame: `Full`, `Reduced` or `Off` (see "IK level of detail" below). |

Status codes (not agent methods; read them off the component):

| Code | When |
|---|---|
| `foot_probe_missing` | `probe_installed` is false. Feet are not moved, and an effect already on fades out. |
| `foot_legs_unresolved` | `legs_resolved` is false. The feet are not moved. The component does not panic. |

`scene` methods show the pair: `scene.get_entity` on `FootPlacementStatus`. With the component
on the rig, the `play.expect` snippet above asserts `FootPlantReport.max_slide` with no host
insert.

## Heel and toe: stairs and toe bend

`FootPlanter::plan` sees a foot as one ankle point plus a tilt toward one ground normal, probed
under the ankle. A foot half over a stair nosing then floats or sinks its toe into the riser.
Heel and toe samples fix that (ADR-8505). On a rig with `FootPlacement`, set `foot_shape`:

```rust,wrap
use regia_animation::foot_ik::FootShapeSettings;
# let mut feet = regia_animation::foot_ik::FootPlacement::default();

feet.foot_shape = Some(FootShapeSettings::default()); // None (the default) keeps the ankle probe alone
feet.legs[0].toe = Some("ball_l".into()); // a toe joint gives the toe tip and the toe bend
```

The engine then probes the heel and the toe tip of each foot as well as the ankle, at the pin while
one holds the foot. When the two heights differ by more than `step_edge`, one probe halfway between
them tells a ramp from a step, and a short bisection (`edge_steps`, default 5) finds the step edge.

- **Pitch.** The foot pitches to the line from the heel to the toe ground, up to `max_tilt`. Roll
  still comes from the ground normal, so a coarse collision mesh with flat normals still pitches
  the foot on a ramp.
- **Stair rule.** On a step the sole stays flat. With at least `min_support` (default 0.5) of the
  heel-to-toe length over the higher tread, the foot rests on it and the rest hangs past the
  nosing. Otherwise the foot moves along its heel-to-toe axis onto the tread it mostly covers,
  never more than `max_edge_shift` (default 0.12 m). A move that would need more, or that would
  take the foot out of the leg's reach, rests the foot on the higher tread instead.
- **No sinking.** A planted heel, ball or toe tip is never placed under the ground it was probed
  over. While such a leg is planted the pelvis sinks as far as its reach needs at once (not at
  `max_slew`), so a step down never leaves the leg short.
- **Toe bend.** With a toe joint, a heel the clip lifts during stance exit turns the foot about the
  ball. The toe joint counter-rotates, so the toe tip stays on the ground until the ball lifts.

Pose output keeps each foot's clip orientation turned by its tilt (`apply_foot_plant_with_toes`),
so the solved heel and toe land where the plan put them. Graph output (`GraphParams`) gets the
moved and lifted targets as parameters before evaluation; after the graph's `TwoBoneIK` nodes
solve, the engine turns each foot joint by its tilt (in object space, on the solved foot) and bends
each toe joint, in the pose-output slot of `apply_animation` (ADR-10130). Flat feet with no toe bend
leave the evaluated pose as it is. The graph's own `TwoBoneIK` nodes keep the foot on the shin, so
unlike pose output the tilt is applied to that solved orientation, not the clip's.

`step_edge` (0.04 m), `min_support` (0.5) and `max_edge_shift` (0.12 m) are `FootPlantSettings`.
`FootShapeSettings`: `heel_length` 0.06 (heel behind the ankle), `toe_length` 0.05 (tip in front of
the toe joint), `front_length` 0.17 (tip in front of the ankle when the leg names no toe joint),
`edge_steps` 5 (0 to 8). At most `3 + edge_steps` probes per foot, two on level ground. A toe name
that does not resolve uses `front_length` and never bends; it does not fail `legs_resolved`.

Hosts that plan by hand pass one `FootSole` per leg (`probe_foot_sole` builds it from any ground
query) to `FootPlanter::plan_with_soles`, and pose with `apply_foot_plant_with_toes`.
`FootSole::default()`, or no entry, plans the leg exactly as `plan` does, bit for bit.

`FootPlantReport` gains two per-leg fields:

| Field | Meaning |
|---|---|
| `heel_gap` | Height of the planted heel over the surface under it, meters. Negative is under the ground. A heel hanging past a step edge the sole rests on is measured to that edge's tread. 0 while the foot swings or without heel and toe samples. |
| `toe_gap` | The same for the toe tip. |

A check that a character on stairs neither floats nor sinks: step the game while it climbs and
assert both gaps of the planted leg stay within 1 cm.

```json
play.expect { "assertions": [
  { "condition": { "entity": 4294967310, "component": "FootPlantReport", "path": "heel_gap[0]", "op": "lte", "value": 0.01 } },
  { "condition": { "entity": 4294967310, "component": "FootPlantReport", "path": "toe_gap[0]", "op": "lte", "value": 0.01 } },
  { "condition": { "entity": 4294967310, "component": "FootPlantReport", "path": "heel_gap[0]", "op": "gte", "value": -0.001 } },
  { "condition": { "entity": 4294967310, "component": "FootPlantReport", "path": "toe_gap[0]", "op": "gte", "value": -0.001 } } ] }
```

No new status code: a probe that misses under the heel or the toe plans that leg from the ankle
probe alone, and both gaps read 0.

The engine-side acceptance is `cargo test -p regia_animation --test foot_stairs`: a sweep of foot
positions across a 0.18 m riser, the demo walk up and down ten steps (0.18 m rise, 0.28 m run;
planted heel and toe never under a tread, never more than 1 cm over it), the pitch on 5 to 25
degree ramps, the toe bend during a 40 degree heel roll, and bit identity of the single-probe
planter.

## IK level of detail

`FootPlacement` and `GaitRig` have the same `lod: IkLodSettings` (ADR-8728). Each frame a rig
picks one `IkLod` from its distance to the `AnimLodCamera` resource (the rig entity's translation to
the camera position), or from a host-set value:

| `IkLod` | When | What runs |
|---|---|---|
| `Full` | within `full_within` (default 30 m), or no `AnimLodCamera` | Probe the ground and plan, every frame. |
| `Reduced` | beyond `full_within`, within `reduced_within` (default 80 m) | Probe every `reduced_interval` frames (default 4), reuse the last hits in between. The plan and the leg solve still run every frame, so the feet do not stutter. |
| `Off` | beyond `reduced_within` | No probes. Foot placement fades out on the cached hits at `blend_out_per_s`, then leaves the clip pose untouched (no plan, no apply). A `Driven` `GaitRig` is neither stepped nor posed and starts from rest when it comes back; an `Integrating` rig keeps stepping at the reduced cadence (it moves its own transform) and only its leg solve stops. |

`force: Some(IkLod::…)` wins over the distance (a cutscene that needs every foot, a budget manager
that knows better). A non-finite `full_within` keeps the rig at full detail at any distance; a
`reduced_within` at or below `full_within` has no reduced band. Reduced probing is staggered by the
entity index, so a crowd that crosses the boundary together does not probe on one frame. A
`GaitRig` at reduced detail answers the stepper from the plane through the nearest of its last 16
hits.

<!-- ignore-reason: fragment of a host's setup; `rig` and `feet` stand for the host's own entity and component -->
```rust,ignore
use regia_animation::ik_lod::{IkLod, IkLodSettings};
use regia_animation::AnimLodCamera;

app.insert_resource(AnimLodCamera { position: camera_translation }); // update it every frame
feet.lod = IkLodSettings { full_within: 20.0, reduced_within: 60.0, reduced_interval: 4, force: None };
feet.lod.force = Some(IkLod::Full); // e.g. for a close-up
```

Read and assert the level as an agent. Both statuses carry it:

```json
{"method":"scene.get_entity","params":{"entity":42,"components":["FootPlacement","FootPlacementStatus"]}}
{"method":"scene.set_property","params":{"entity":42,"component":"FootPlacement","path":"lod.full_within","value":5.0}}
{"method":"play.wait_until","params":{"condition":{"entity":42,"component":"FootPlacementStatus","path":"lod","op":"eq","value":"Reduced"},"timeout_ms":2000}}
{"method":"play.expect","params":{"assertions":[
  {"name":"the hexapod is full detail","condition":{"entity":43,"component":"GaitRigStatus","path":"lod","op":"eq","value":"Full"}}]}}
```

`scene.set_property` on `FootPlacement.lod.full_within` (or `GaitRig.lod.full_within`) changes it
live. No new status code and no new method: `Off` is a level, not a failure.

### Cost

A settled frame allocates nothing: the planter fills a kept `FootPlant` (`FootPlanter::plan_into`),
the report is refilled in place (`FootPlanter::report_into`), and the apply refreshes forward
kinematics along each leg chain in a kept buffer (`apply_foot_plant_into`) instead of rebuilding
every joint per leg. Every per-frame entry point has a `tracing` span: `foot_placement`,
`foot_plant_plan`, `apply_foot_plant`, `gait_rig_step` and `gait_rig_pose`.

`cargo run --release -p regia_animation --example locomotion_cost -- --scaling` walks bipeds with
`FootPlacement` and hexapods with `GaitRig` on a rolling analytic ground for 2000 frames and prints
the locomotion time per frame (p50, p99) and the whole frame with and without the components. The
numbers per device are in `docs/perf/locomotion-cost.md`; they are evidence, never a test.

The engine-side acceptance is `cargo test -p regia_animation --test locomotion_cost` (no allocation
after warm-up, `plan_into` bit for bit with `plan`, the reduced cadence and the clip pose when off,
for both components) and `cargo test -p regia_animation --lib foot_ik::plant` (at most one
skeleton-wide forward-kinematics pass per apply, bit for bit with the old per-leg rebuild).

## See it: the locomotion overlay

The `locomotion` gizmo group (ADR-8729, off by default) draws what the planter did this frame for every
`FootPlacement` rig, with the `foot_placement` sub-toggle:

```json
debug.set_gizmos { "group": "locomotion", "enabled": true, "toggles": { "gait": false } }
debug.gizmos {}
→ { "groups": [ ..., { "name": "locomotion", "enabled": true, ...,
      "toggles": { "gait": false, "foot_placement": true, "ik": true } }, ... ] }
```

| Drawn | Color token | Reads |
|---|---|---|
| each leg's probe ray, from the pin (else the ankle) `probe_height` up, `probe_length` down | `color.text-muted` | the same ray the planter casts, through `GroundProbes` |
| the probe hit (a cross) and the ground normal | `color.focus-ring`, `color.accent` | the probe's `GroundHit` |
| the tilted sole: a ring at the hit, tilted from up toward the normal by the leg's tilt | `color.success` | `FootPlantReport.tilt_deg[i]` |
| each pin (a ring) with a bar as tall as its lock weight (0.3 m at 1) | `color.info` | `FootPlacement::pin`, `FootPlantReport.lock[i]` |
| a ring as wide as the foot's slide since touchdown | `color.danger` | `FootPlantReport.slide[i]` |
| the pelvis offset: a cross at the root joint and a line to where it would be without the offset | `color.warning` | `FootPlantReport.pelvis_offset` |

With the `ik` sub-toggle each leg draws its hip, knee and ankle chain, the plan's ankle target (pose mode;
the pin in graph mode), the knee pole and the leg's reach sphere (thigh plus shin).

A slide ring that keeps growing while a foot is planted means the lock is not holding (compare
`FootPlantReport.max_slide`); a probe ray that ends in the air means `probe_length` is too short for that
step. Acceptance: `on_draws_the_expected_primitive_counts_for_a_biped` in
`regia_framework --test locomotion_gizmos`.

## Worked scene: a biped on stairs

The anim_lab locomotion scene (`anim_lab::locomotion`, ADR-8807) has a `FootPlacement` biped with heel and
toe probes (`foot_shape` set, toe joints named) that walks up ten stairs (0.18 m rise, 0.28 m run)
again and again with the demo walk played in place, while a scripted mover moves its root along the tread
middles. A respawn (farther than `teleport_distance`, so the pins clear) takes it back to the bottom after
each climb. Walking down is not in the acceptance yet (ADR-8807, known issue). One `GroundProbe` (`anim_lab::locomotion::Course`) serves it and the gait rig next to it. Find it
with `scene.query_entities { "component": "FootPlacement" }`, step it with `play.step`, and read
`anim.locomotion_state` (`foot_plant.max_slide`, `heel_gap`, `toe_gap`, `foot_placement.lod`) or assert on
`FootPlantReport` paths with `play.expect`. Acceptance: `locomotion_scene_acceptance` in
`cargo test -p anim_lab --test locomotion_acceptance` (a respawn and its 30 settle frames excepted:
planted feet slide less than 1 cm, planted heels and toes never more than 1 mm under the tread) and
`locomotion_agent_dogfood` in `--test locomotion_agent`. The one-frame ankle jump (8 cm) and heel/toe float
(1 cm) bounds are in `locomotion_biped_stairs_have_no_pops`, ignored for now: climbing these stairs, an
ankle pops 0.24 m in one frame and a planted sole floats 2 cm (ADR-8807, known issue).
