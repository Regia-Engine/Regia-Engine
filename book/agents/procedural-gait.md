# Procedural gait

This page was "Six-leg tripod gait" (`agents/tripod-gait.md` now redirects here); the guide
topic `tripod-gait` still resolves, to `procedural-gait`. The stepper drives any leg count from
1 to 16 with any gait pattern (ADR-8653, see "Gait patterns: any leg count and gait" below); the
six-leg alternating tripod described first is its `GaitPattern::tripod()` preset.

`regia_animation::gait` computes six foot targets and a body position from forward,
strafe and steer controls and a floor-height callback. Add `AnimationPlugin`, construct
`GaitState::at_rest` with explicit `GaitParams`, and attach the state to your rig entity.
Call `step_gait` from your host's presentation update with a small fixed timestep. Feed
its six targets into your leg IK. The plugin registers reflection; it does not schedule
stepping, move the entity, query collision, or solve the rig automatically.
A rig with the `GaitRig` component is walked by the engine instead (ADR-8707, see "GaitRig: the
engine walks the rig" below).

Coordinates are meters, +Y up, +X right and **+Z forward at zero yaw**. Convert once
at your host boundary if your rig faces -Z. Feet are front-left, front-right,
middle-left, middle-right, rear-left, rear-right. One tripod is `[0, 3, 4]`, the other
`[1, 2, 5]`. At least three feet remain planted; at idle the feet step back to neutral hips
(see "Stopping and turning in place"). Planted XZ stays fixed, but Y follows the floor sampler. With
`posture: GaitPosture::UPRIGHT` body Y is mean planted height plus clearance and the body never tilts;
other postures smooth the height and pitch and roll the body (see "Posture: body height and tilt").
A refused slope holds the previous plant even on touchdown.
Slope refusal does not stop body travel; this is not a collision or balance controller.
The stepper does not enforce a cross-slope limit.

All geometry and motion settings are host supplied. There are no game-specific defaults.
Settings are finite, with positive `acceleration` and `step_length`, nonnegative other
scalar settings and finite `hip_offsets`. Controls are finite and clamped to `[-1, 1]`.
A timestep advancing more than one full tripod period is refused; subdivide it.
Finite zero/negative time does nothing. Power loss freezes everything and calls no floor
sampler. `shift_origin` subtracts an origin shift from body, feet and lift-off history;
the host must call it when it rebases its own coordinate space.

## Inspect and test through the agent

No new protocol method is required. Existing methods use the registered component:

```json
{"method":"scene.query_entities","params":{"component":"GaitState"}}
{"method":"scene.get_entity","params":{"entity":42,"components":["GaitState"]}}
{"method":"play.expect","params":{"assertions":[{"name":"tripod B is planted","condition":{"entity":42,"component":"GaitState","path":"feet[1].planted","op":"eq","value":true}}]}}
{"method":"scene.set_property","params":{"entity":42,"component":"GaitState","path":"power_lost","value":true}}
```

Use the entity id from the query instead of `42`. `scene.get_entity` returns position,
yaw, velocity, phase, active tripod, power flag, feet, and lift-off history. A host that
runs stepping during its frames can use `play.step` and `play.wait_until` with the same
field predicates. `agent.guide {"topic":"tripod-gait"}` explains the workflow.
`crates/regia_agent/tests/tripod_gait.rs` exercises inspection, contact assertions,
freezing and guide discovery through the dispatcher without a window or GPU.

The Rust errors are `gait_settings`, `gait_input`, `gait_state`, `gait_floor`,
`gait_step_too_large` and `gait_pattern` (ADR-8653, see below): fix settings, finite inputs/state/floor results, or subdivide time,
respectively. A refused call leaves state unchanged, although floor callbacks may have
run. These are library errors, not new RPC errors; generic scene methods keep their
existing error contracts. Do not edit history or phase to advance animation: call the
stepper. It validates reflected state before using it.

## Determinism and adoption

This is presentation code using Regia's deterministic math and plain f32 arrays, with
no allocations per step. It does not claim bit identity with a simulation's math pin.
The source scenarios are adapted from the owner-approved DO27 snapshot `d6ed37677`
(regia-absorb-1); dimensions and speeds in the tests are synthetic. Before adopting it
inside lockstep, use the approved simulation math and verify the complete simulation's
checksums. Existing simulation code stays until the host's compatibility checks pass.

## Driven by your simulation

When gameplay moves the walker (a simulation, a physics mover or navigation), do not let the
stepper move it. Construct with `GaitState::at_rest_driven(&params, body, floor)` and call
`step_gait_driven(&params, &mut state, body, dt, floor)` each presentation step with the body your
simulation computed: `GaitBody { position, yaw, velocity }`. X, Z, yaw and velocity are copied into
the state bit for bit; body Y stays the stance height. The stepper only advances the tripod phase
from the body's ground speed, places feet and follows stance height (ADR-8474).

| quantity | `step_gait` (integrating) | `step_gait_driven` (driven) |
|---|---|---|
| position, velocity | world meters, +Y up | world meters, +Y up, copied (body Y ignored) |
| yaw | rotation about +Y | rotation about +Y, same sense, copied |
| local forward | +Z: `[sin(yaw), 0, cos(yaw)]` | -Z (object convention): `[-sin(yaw), 0, -cos(yaw)]` |
| `hip_offsets[i]` | `[+X, +Z]` local | `[right (+X), forward (-Z)]` local |

Offsets written for `step_gait` give the same feet in driven mode at the same yaw values after
negating each offset's second component. In driven mode the foot names (front-left first) hold in
Regia's right-handed world.

A body that moved farther than `step_length` horizontally in one step (teleport, an origin shift by
the simulation, lag correction) replants every foot at neutral instead of stretching the legs, and
`replants` counts it. Switching a state between the two modes replants once and counts too. `mode`
(`Integrating` or `Driven`) records which entry point last advanced the state. Both fields are
reflected, so a walk can be checked through the existing methods:

```json
{"method":"play.expect","params":{"assertions":[{"name":"driven by the sim","condition":{"entity":42,"component":"GaitState","path":"mode","op":"eq","value":"Driven"}},{"name":"no pops over the walk","condition":{"entity":42,"component":"GaitState","path":"replants","op":"eq","value":0}}]}}
```

A rising `replants` during a continuous walk means the simulation body jumped (or the host
mixed both entry points); check the sim's corrections before tuning the gait. The same Rust errors
apply: a nonfinite body or timestep is `gait_input`; a ground speed that would advance more than one
tripod period is `gait_step_too_large` (subdivide `dt` and interpolate the body). Never derive
gameplay footsteps (stomp damage, noise heard by AI) from these feet: those come from the
simulation's own cadence. `crates/regia_agent/tests/tripod_gait.rs` asserts `mode` and `replants`
through the dispatcher.

## Stopping and turning in place

At or below `idle_speed` (both modes) the feet step instead of snapping (ADR-8539):

1. The swing in progress finishes on its arc, its landing bent smoothly to neutral (the hip
   under the body). The bend moves a landing at most `2 * step_length` per `settle_period`
   (ADR-10130), so a stop that catches a swing late (past about three quarters of its arc)
   stretches that swing, by at most a quarter of `settle_period` for a walking landing, and it
   still lands at neutral (ADR-10162). A dead stop therefore settles within two `settle_period`s
   and no foot jumps toward neutral in one frame.
2. Each tripod with a foot farther than `settle_distance` from its neutral then takes a normal
   swing there, in tripod order, one tripod at a time, each swing lasting `settle_period`.
3. When no swing is in progress and no foot is past its threshold, `settled` is true. A settled
   foot never moves: only its height follows the floor sampler.

Turning in place (ground speed at or below `idle_speed`, yaw changing) uses the same steps with
the turn thresholds: a planted foot farther than `turn_step_distance` from its rotated neutral,
or whose direction from the body center differs from its rotated neutral's by more than
`turn_step_angle` radians, makes its tripod step toward the rotated neutral. The other tripod
waits, so at least three feet are always planted, and a planted foot holds its world XZ until it
steps. The turn thresholds hold for `settle_period` seconds after the last step whose yaw changed
(ADR-10130), so a host that hands the driven stepper a new yaw on only some steps (a 20 Hz heading
under a 60 Hz presentation) turns by the same turn steps as one that updates yaw every step; once
yaw has not changed for `settle_period`, the feet settle with `settle_distance`. A foot whose
neutral is refused (steeper than `max_slope` from where it stands, no ground, out of reach) and
whose nearest valid ring foothold is within `settle_distance` of where it stands stays put and
counts as settled: it is as close to neutral as the ground allows, and stepping would only step in
place (ADR-10130 keeps this rule).

Settings: `settle_distance` and `turn_step_distance` are nonnegative meters, `settle_period` is
positive seconds, `turn_step_angle` is in `[0, pi]` radians (`pi` leaves only the distance test).
Anything else is `gait_settings`. An idle step that would run a swing more than one
`settle_period` is `gait_step_too_large`, as when walking; a settled gait accepts any finite
timestep.

`settled` is reflected, so an agent checks a stop like this: after stopping, `settled` becomes
true within 2 periods (`2 * settle_period` seconds of steps), and no foot moves after.

```json
{"method":"play.step","params":{"frames":40}}
{"method":"play.expect","params":{"assertions":[{"name":"settled after the stop","condition":{"entity":42,"component":"GaitState","path":"settled","op":"eq","value":true}}]}}
{"method":"scene.get_entity","params":{"entity":42,"components":["GaitState"]}}
{"method":"play.step","params":{"frames":120}}
{"method":"play.expect","params":{"assertions":[{"name":"still settled","condition":{"entity":42,"component":"GaitState","path":"settled","op":"eq","value":true}},{"name":"foot 0 x holds","condition":{"entity":42,"component":"GaitState","path":"feet[0].position[0]","op":"eq","value":1.25}}]}}
```

Step the frames your host's rate gives for `2 * settle_period` (40 covers 0.3 s periods at 60
frames per second with margin; `play.wait_until` with a `timeout_ms` works too), and compare the
`feet[i].position` values that `scene.get_entity` returned. `crates/regia_agent/tests/tripod_gait.rs`
asserts `settled` through the dispatcher, and `regia_animation --test tripod_gait` proves the swing
bound for every foot on every step (`no_foot_jumps_more_than_its_swing_bound`).

## Ground: layers, holes, cliff edges and reach

The steppers take any `GaitGround` (ADR-8792), not only a height closure:

```rust
use regia_animation::gait::GroundHit;

pub trait GaitGround {
    fn sample(&self, x: f32, z: f32, expected_y: f32, band: f32) -> Option<GroundHit>;
}
```

`expected_y` is where the walker's floor should be: the body height target
(`GaitState::body_height_target`) minus `clearance`; for `at_rest`, `at_rest_driven` and a driven
replant, the body Y your simulation passed. The visual body height may lag the target under a slow
posture ease; the query follows the target, so a climbing walker keeps its footholds (ADR-10053). `band` is the setting
`ground_band`. Answer the layer nearest `expected_y` within the band, so a walker in a tunnel gets
the tunnel floor and not the terrain above it, and a walker under a bridge gets the ground, not the
deck. Return `GroundHit { y, normal }` (any nonzero normal length; it is normalized) or `None` where
there is no ground. A height outside the band is also treated as no ground. Every
`Fn(f32, f32) -> f32` closure is still a ground (normal +Y), so old call sites compile; only a closure
whose untyped parameters are used through a method call (`|x, z| x.sin()`) needs `|x: f32, z: f32|`.
Adapters are yours to write: a physics raycast down from `expected_y + band`, a terrain heightfield
(`regia_animation --test tripod_gait` has one over a code-built grid), a multi-layer floor query. The
animation crate never calls physics or terrain.

What the stepper does with the answers:

- **Footholds.** A landing is refused when there is no ground, the height is outside the band, the
  surface normal or the step from lift-off is steeper than `max_slope`, or the point is beyond the
  leg's `max_reach`. It then tries `foothold_candidates` points (at most 64) on rings
  `foothold_spacing` apart around the landing, 8 per ring in a fixed order in the body frame
  (right, front-right, front, front-left, left, back-left, back, back-right), and takes the valid
  point of the nearest ring with the smallest height change. With none, the foot holds its lift-off
  point as before and `refused` counts it.
- **Reach.** `max_reach[i]` is the 3D distance from leg `i`'s hip (under the body, at body height).
  A planted foot beyond it takes a forced step out of phase, lasting `settle_period`, and
  `forced_steps` counts it. After every step no planted foot is beyond reach. Fewer than three feet
  are planted only during a forced step or while a foot hangs.
- **No ground.** A foot with nowhere to stand hangs `hang_drop` below the expected floor under its
  hip with `planted == false`. It is not an error. `refused` counts it, and the foot lands again at a
  later swing that finds a foothold. With nothing planted, the body height holds.
- **Normals.** `feet[i].normal` is the unit ground normal under a planted foot (at the landing for
  a swinging foot, +Y while hanging), for foot tilt in your IK.

`foot_modes[i]` is `Tripod`, `Forced` (in a forced step), `Waiting` (landed a forced step, planted
until its tripod lands) or `Hanging`. All of it is reflected. On a walk over valid ground with a
reach longer than the stride, `forced_steps` stays 0. Assert that, and that a foot is on the ground:

```json
{"method":"play.step","params":{"frames":120}}
{"method":"play.expect","params":{"assertions":[{"name":"no forced steps on the flat","condition":{"entity":42,"component":"GaitState","path":"forced_steps","op":"eq","value":0}},{"name":"no refused footholds","condition":{"entity":42,"component":"GaitState","path":"refused","op":"eq","value":0}},{"name":"foot 0 on level ground","condition":{"entity":42,"component":"GaitState","path":"feet[0].normal[1]","op":"eq","value":1.0}}]}}
{"method":"play.expect","params":{"assertions":[{"name":"right front foot hangs at the edge","condition":{"entity":42,"component":"GaitState","path":"foot_modes[1]","op":"eq","value":"Hanging"}}]}}
```

A rising `forced_steps` on ground that should be walkable means `max_reach` is shorter than the
stride (a foot planted at neutral trails its hip by up to one `step_length`, so give at least
`sqrt(step_length^2 + clearance^2)` with some margin), or the ground
adapter is answering the wrong layer: check `ground_band` and the `expected_y` your adapter
receives. A rising `refused` near edges and holes is expected. Settings: `ground_band` > 0
(infinity accepts any height), `foothold_candidates` 0 to 64 (0 disables the search),
`foothold_spacing` > 0, `max_reach` > 0 per leg (infinity disables it), `hang_drop` >= 0; anything
else is `gait_settings`. A nonfinite height or normal, or a zero normal, is `gait_floor`.
`crates/regia_agent/tests/tripod_gait.rs` asserts `forced_steps`, `refused`, a foot normal and a
hanging foot through the dispatcher.

## Gait patterns: any leg count and gait

ADR-8653. `GaitParams::patterns` is a `GaitPatternSet` of one to four `GaitPattern`s, slowest first.
A pattern is `{ legs, phase_offset, duty_factor }`: foot `i`'s phase is `(cycle + phase_offset[i])
mod 1`, phase 0 is touchdown, the foot is in stance while its phase is below `duty_factor` and swings
from lift-off to its landing on the usual arc. Per-foot arrays (`hip_offsets`, `max_reach`, `feet`,
`foot_modes`, `foot_phase`) are `MAX_LEGS` (16) long and only the first `legs` entries are used; build
them with `leg_array([...], fill)`. `step_length` is the body travel during one stance, so a stride
is `step_length / duty_factor`.

Presets (gait diagrams from the literature, not game tuning). Leg order is pairs front to back, left
before right; quadrupeds are left fore, right fore, left hind, right hind:

| constructor | legs | duty | swings |
|---|---|---|---|
| `GaitPattern::tripod()` | 6 | 0.5 | feet 0, 3, 4 together, then 1, 2, 5 (the original stepper, bit for bit) |
| `GaitPattern::ripple(n)` | even, 4..=16 | `1 - 2/n` | each side back to front, the right side half a cycle later |
| `GaitPattern::wave(n)` | even, 2..=16 | `(n-1)/n` | one foot at a time: left back to front, then right back to front |
| `GaitPattern::quadruped_walk()` | 4 | 0.75 | left hind, left fore, right hind, right fore |
| `GaitPattern::trot()` | 4 | 0.5 | diagonal pairs |
| `GaitPattern::pace()` | 4 | 0.5 | lateral pairs |
| `GaitPattern::bound()` | 4 | 0.4 | fore pair, then hind pair (flight phases) |
| `GaitPattern::biped_walk()` | 2 | 0.6 | left, right (double support) |

`GaitPattern::new(&offsets, duty)` builds any other pattern. Speed switching:
`GaitPatternSet::new(&[walk, trot, bound], &[1.0, 2.0], 0.1)` switches up past a switch speed plus the
hysteresis and back below it minus the hysteresis, one neighbor at a time and only while walking. A
switch blends offsets (the short way around) and duty over one cycle; neighboring duty factors differ
by at most `MAX_DUTY_STEP` (0.5), so every foot's phase keeps advancing and no foot jumps. A switch still
blending when the walker stops ends at once: feet in the air finish as forced steps.

Idle settling and turning in place work per lift-off group: feet with the same lift-off time step
together, one group per `settle_period`, in pattern order.

Reflected `GaitState` fields for agents: `legs`, `pattern` (the pattern in effect, mid-switch the
blend), `pattern_index`, `blending`, `blend`, `beat` (the lift-off group whose turn it is), `phase`
(progress through that beat), `foot_phase[i]` and `stability_margin`: the signed XZ distance in meters
from the body center to the support polygon of the planted feet (convex hull), positive inside,
negative outside, and `NO_SUPPORT_MARGIN` (`-3.4028235e38`, the most negative finite `f32`) with nothing
planted, so it is always a JSON number and `lt`/`lte` checks hold during flight (ADR-8736). A trot, pace or
bound plants at most two feet, so its margin is never positive: statically unstable by design; a bound also
has flight phases with no foot planted. To tell flight from a poor stance, check
`stability_margin` `gt` `-1e30` or read `feet[i].planted`.
`regia_animation::gait::support_margin(&points, center)` computes the same value for any points.

```json
{"method":"play.expect","params":{"assertions":[
  {"name":"walking a trot","condition":{"entity":42,"component":"GaitState","path":"pattern_index","op":"eq","value":1}},
  {"name":"statically unstable","condition":{"entity":42,"component":"GaitState","path":"stability_margin","op":"lte","value":0.0}},
  {"name":"foot 0 in stance","condition":{"entity":42,"component":"GaitState","path":"foot_phase[0]","op":"lt","value":0.5}}]}}
```

Error `gait_pattern` (Rust `GaitError::Pattern`): `legs` is 0 or above 16, an offset is not finite or
outside `[0, 1)`, a duty factor is outside `(0, 1)`, the patterns of a set drive different leg counts,
the switch speeds are not positive, increasing and more than twice the hysteresis apart, the hysteresis
is negative, neighboring duty factors differ by more than 0.5, or the settings drive another leg count
than the state was built for. The state's own reflected `pattern` is checked the same way, so an agent
`scene.set_property` of `pattern.duty_factor` to 0 makes the host's next step refuse with `gait_pattern`
(ADR-8736), and a `pattern_index` past the set (a host shrank its set) is refused until the state is
rebuilt. Fix the pattern or rebuild the state; a refused call changes nothing. A driven body velocity whose
ground speed overflows `f32` is `gait_input`.

Walking from rest with overlapping swings (ripple, bound), feet the pattern has in the air at the start
lift on the first step and land on schedule, so `feet[i].planted` agrees with `foot_phase[i]` from the
first step. During a switch a foot whose swing is shorter than one host step still steps (lifts and lands
in that step), so contact counts do not depend on the frame rate.

Acceptance: `tripod_preset_matches_the_six_leg_stepper_bit_for_bit`, `duty_factor_sets_the_stance_fraction`,
`wave_gait_keeps_n_minus_one_feet_planted`, `trot_plants_diagonal_pairs`, `pace_plants_lateral_pairs`,
`pattern_switch_keeps_every_foot_continuous`, `two_and_sixteen_legs_work`, `invalid_pattern_is_refused`,
`stability_margin_matches_a_hand_computed_case`, `foot_phase_follows_the_pattern` and
`pattern_steps_allocate_nothing`, `pattern_equality_ignores_unused_entries`,
`reflected_blend_patterns_are_validated`, `no_support_margin_is_finite_and_below_every_margin`,
`walk_from_rest_lifts_feet_the_schedule_has_in_the_air`, `first_step_crossing_a_beat_steps_the_starting_group`
and `overflowing_body_speed_is_an_input_error` in
`regia_animation --test gait_patterns`; the review proofs in `regia_animation --test gait_patterns_review`;
`pattern_fields_are_asserted_through_dispatcher` and `flight_margin_and_pattern_writes_through_dispatcher` in
`regia_agent --test tripod_gait`.

## GaitRig: the engine walks the rig

`regia_animation::gait_rig::GaitRig` (ADR-8707) turns the stepper into an engine-side walker. Put it on a
rig entity (one with `SkinnedMesh` and `Animator`) and the engine schedules the steps, converts axes,
measures the hip offsets from the bind pose, asks the ground, and solves the legs. Your code only moves
the entity (driven mode) or writes controls (integrating mode).

<!-- ignore-reason: fragment of a host's setup; `params` stands for the host's GaitParams -->
```rust,ignore
use regia_animation::gait_rig::{GaitLeg, GaitRig, GaitRigMode};

// Legs in pattern order (pairs from the front, left before right), named root joint to tip joint.
let legs = ["fl", "fr", "ml", "mr", "rl", "rr"]
    .iter()
    .map(|leg| GaitLeg::new(format!("coxa_{leg}"), format!("foot_{leg}")))
    .collect();
commands.entity(rig).insert(GaitRig::new(params, legs).with_body("thorax"));
```

Each frame, in `PostUpdate` before `apply_animation`, `step_gait_rigs`:

- resolves the legs on first use and after a skeleton or name change (a leg of three joints uses the
  two-bone solve, any other count uses FABRIK with `GaitLeg::limits`);
- measures each leg's neutral foothold from the bind pose
  (`GaitParams::hip_offsets_from_skeleton`, scaled by the rig's scale) unless `auto_hips` is false;
- steps the gait by the frame time (frames longer than 1/30 s are split into equal substeps, at most 8;
  a substep the stepper refuses as longer than one stance or settle period is retried at half the length,
  at most 4 halvings per frame, so a fast gait walks the same at 30, 60 and 144 Hz (ADR-8800); a finest
  slice the stepper still refuses is stepped with the ground speed capped just under one stance per slice,
  so a rig commanded past that speed walks at the cap instead of freezing, and the cap is not a
  `gait_rig_step` error (ADR-10053; `step_length` 0.25 m at 30 Hz caps at about 120 m/s); a driven
  frame whose body moved more than `step_length` replants every foot at neutral, like a teleport),
  grounded through the scene's `GroundProbes` resource (the probe `FootPlacement` uses) cast down
  through `ground_band` around the expected floor; with no probe the ground is the flat plane at the
  rig's floor;
- keeps the result in the rig's `GaitState` component (the engine inserts it) and inserts
  `GaitRigStatus`.

Then `apply_animation`, right after foot placement and before eye gaze, lifts the body joint to the
stance height and solves every leg to its foot target. A rig without `GaitRig` is untouched.

Modes (`GaitRig::mode`): `Driven` (the default) follows the rig's `Transform` (XZ and yaw), with the
velocity from a `GaitVelocity` component or else from the transform's change over the frame.
`Integrating` moves the rig itself from a `GaitControls { forward, strafe, steer }` component (forward is
the rig's −Z, strafe +1 is its right, steer +1 turns left) and writes the rig's translation and yaw.

Conventions: the rig faces −Z in object space, +Y up, uniform scale (`scale.x`). The floor of the bind
pose is the lowest leg tip; a tip that sits higher in the bind pose stays that much above the ground.
The body joint moves by the stance floor (mean planted foot height) minus the rig's floor, so in driven
mode keep the rig's transform on the ground under the body. Body pitch and roll are not applied here.

### Inspect and test through the agent

No new protocol method. `GaitRig`, `GaitState` and `GaitRigStatus` are reflected:

```json
{"method":"scene.query_entities","params":{"component":"GaitRig"}}
{"method":"scene.get_entity","params":{"entity":42,"components":["GaitRig","GaitState","GaitRigStatus"]}}
{"method":"scene.set_property","params":{"entity":42,"component":"GaitControls","path":"forward","value":1.0}}
{"method":"play.wait_until","params":{"condition":{"entity":42,"component":"GaitState","path":"settled","op":"eq","value":false},"timeout_ms":2000}}
{"method":"play.expect","params":{"assertions":[
  {"name":"every leg resolved","condition":{"entity":42,"component":"GaitRigStatus","path":"legs_resolved","op":"eq","value":6}},
  {"name":"no refused step","condition":{"entity":42,"component":"GaitRigStatus","path":"step_errors","op":"eq","value":0}}]}}
```

`scene.get_entity` on `GaitRig` returns `params`, `legs` (`root`, `tip`; limits are not reflected),
`body`, `mode`, `auto_hips`, `ik_iterations`, `ik_tolerance`, `probe_height` and `probe_length`. On
`GaitState` it returns the stepper state described above. `GaitRigStatus` has `probe_installed`,
`legs_resolved`, `issue`, `warnings` and `step_errors`.

`GaitRigStatus::issue` codes (`GaitRigIssue::code`), reported, never a panic:

| code | meaning | what happens |
|---|---|---|
| `gait_rig_leg_unresolved` | a leg's root or tip joint is not on the skeleton, or the tip is not under the root | that leg is not solved (its joints keep the clip pose); the other legs walk |
| `gait_rig_body_unresolved` | the body joint is not on the skeleton | the body is not lifted; the legs still solve |
| `gait_rig_leg_count` | the number of legs is not the pattern set's leg count (or is above 16) | nothing is stepped or solved |
| `gait_rig_step` | the stepper refused this frame (`gait_settings`, `gait_input`, `gait_floor`, a stance shorter than 1/480 s, ...), or `ik_tolerance`, `probe_height` or `probe_length` is not finite (ADR-8800; a JSON number past `f32`, such as `1e39`, is stored as infinity) | state and pose stay as they were; `step_errors` counts it. With non-finite rig settings a rig with no state yet is planted at rest on its floor plane (no probe cast) and FABRIK legs are not solved |

`warnings` counts one warning per resolve that left something unresolved, one when the leg count goes
wrong, and one per refused frame (a rig refused every frame gets one warning every frame).

`GaitRig::reset()` (host code) forgets the gait: the next frame rebuilds `GaitState` with every foot
planted at neutral under the rig's current transform. A frame the stepper refuses keeps the reset for
the next frame, and a driven rig's derived velocity is measured from the last frame that stepped
(ADR-10053).

Acceptance (`regia_animation --test gait_rig`): `gait_rig_feet_reach_their_targets_within_1mm` (two-bone
legs, 600 frames up a 10 degree slope), `four_segment_legs_use_fabrik_and_reach_within_1mm`,
`hip_offsets_from_skeleton_match_the_bind_pose` (both stepper conventions),
`integrating_rig_walks_forward_from_controls`, `a_rig_without_gait_rig_is_bit_identical`,
`missing_leg_joint_reports_and_skips` and `gait_rig_allocates_nothing_after_warmup`;
`gait_rig_is_inspected_and_driven_through_dispatcher` in `regia_agent --test gait_rig`.

## Posture: body height and tilt

ADR-8706. `GaitParams::posture` is a `GaitPosture`. `GaitPosture::UPRIGHT` keeps the stepper as it
was: body Y is the plain mean of the planted feet plus `clearance` (it jumps when the lift-off group
changes on uneven ground) and the body never tilts. Any other posture does two things:

- **Height.** The target `body_height_target` is a weighted mean of every foot's floor plus `clearance`:
  a planted foot counts at its plant height with weight 1, a foot in the air at the ground line under
  it (its lift-off height blended to its landing height by swing progress) with weight `swing_weight`.
  Body Y eases toward the target with time constant `height_tau` (seconds) and moves at most
  `height_slew` meters per second, so it never jumps at a group swap.
- **Tilt.** Pitch and roll come from a least-squares plane through the planted feet in the stepper's
  frame. When the planted feet lie on a line (a trot's diagonal pair, a biped) the fit adds the feet in
  the air at their ground line; if those are on a line too only the slope along it counts; with every
  foot at one spot the tilt holds. The angles are clamped to `max_pitch` and `max_roll`, multiplied by
  `attitude_weight` and eased with `attitude_tau`. After the ease the stored `pitch` and `roll` are
  clamped again to `max_pitch * attitude_weight` and `max_roll * attitude_weight`, so a lowered limit or
  an angle a scene stored past it holds on the next step, and `attitude_weight` 0 writes the identity on
  that step (ADR-10053).

| setting | range | meaning |
|---|---|---|
| `swing_weight` | `[0, 1]` | weight of a foot in the air in the height target (1 = continuous through lift-off and touchdown) |
| `height_tau` | seconds, `>= 0` | height ease time constant; 0 follows the target at once |
| `height_slew` | m/s, `> 0` | most the body height changes per second; infinity for no limit |
| `attitude_weight` | `[0, 1]` | fraction of the ground's tilt the body takes; 0 = upright |
| `max_pitch`, `max_roll` | radians, `[0, pi/2)` | tilt limits before the weight |
| `attitude_tau` | seconds, `>= 0` | tilt ease time constant |

Reflected `GaitState` fields: `body_height_target` (meters), `pitch` (radians, positive raises the nose),
`roll` (radians, positive raises the body's local +X side, the feet with `hip_offsets[i][0] > 0`) and
`attitude`, a quaternion `[x, y, z, w]` in the stepper's local frame (+X right, +Y up, +Z forward in
integrating mode and back in driven mode): pitch about +X, then roll about the pitched forward axis.
Compose it after the yaw: world rotation = yaw rotation x `attitude`. It is exactly `[0, 0, 0, 1]` on
level ground and with `attitude_weight` 0. Tilt is output only: it never moves a foot, so with the
height settings of `UPRIGHT` the body and feet trace is bit for bit the upright one.

A walker on a 10 degree ramp (driven, walking uphill):

```json
{"method":"play.expect","params":{"assertions":[
  {"name":"nose up on the ramp","condition":{"entity":42,"component":"GaitState","path":"pitch","op":"gt","value":0.17}},
  {"name":"not past 10.5 degrees","condition":{"entity":42,"component":"GaitState","path":"pitch","op":"lt","value":0.184}},
  {"name":"tilted","condition":{"entity":42,"component":"GaitState","path":"attitude[3]","op":"lt","value":1.0}}]}}
```

A posture value outside its range is `gait_settings` (Rust `GaitError::Settings`); a refused call
changes nothing. A non-finite `pitch`, `roll`, `attitude` or `body_height_target` in a stored state (a
RON scene can carry NaN) is replaced before the step (0, 0, the identity, the body height) and rebuilt
by it, so the gait keeps stepping and the pose stays finite (ADR-10053). No new RPC method or error code.

Acceptance: `body_height_is_continuous_across_group_swaps`, `body_pitches_with_a_slope_up_to_the_limit`,
`flat_ground_attitude_is_identity_bit_for_bit`, `attitude_weight_zero_matches_the_upright_trace`,
`collinear_feet_fall_back_without_nan`, `invalid_posture_is_refused`, `origin_shift_moves_the_height_target`,
`a_replant_snaps_height_and_tilt` and `posture_steps_allocate_nothing` in `regia_animation --test gait_posture`;
`posture_fields_are_asserted_through_dispatcher` in `regia_agent --test tripod_gait`.

## Gait files: `.gait.ron`

Gap G7. A walker's settings, patterns and legs can live in a `<name>.gait.ron` file (format `gait` v1,
[docs/formats/gait-ron.md](../../docs/formats/gait-ron.md)) instead of a Rust struct literal, so a
designer tunes a walker without code and the file is versioned like every other content format.

```ron
(
    format_version: 1,
    patterns: [Wave(6), Ripple(6), Tripod],
    switch_speeds: [0.5, 1.2],
    hysteresis: 0.1,
    speeds: (forward: 2.0, reverse: 0.8, strafe: 0.6, acceleration: 3.0, idle: 0.03, yaw_rate: 1.2),
    step_length: 0.45,
    swing_lift: 0.15,
    clearance: 0.55,
    max_slope: 1.0,
    max_reach: Some(1.1),
    settle: (distance: 0.05, period: 0.25),
    turn: (step_distance: 0.12, step_angle: 0.35),
    ground_band: Some(0.8),
    foothold: (candidates: 16, spacing: 0.15),
    hang_drop: 0.25,
    body: Some("thorax"),
    legs: [
        (root: "coxa_fl", tip: "foot_fl", hip_offset: Some((-0.45, 0.6))),
        (root: "coxa_fr", tip: "foot_fr"),
        // ... one entry per leg, in pattern order
    ],
)
```

Loading: `GaitAssets` (a resource the animation plugin installs) stores files by key and hands out
handles: `load_file(path)`, `load_text(key, text)`, `get(&handle)`, `generation(&handle)`,
`error(&handle)`. `GaitRig::from_asset(handle, &asset)` builds a rig that follows the handle; a leg with a
`hip_offset` in the file keeps it, the others are measured from the bind pose. When the dev watcher
reports the file changed (`SourceAssetChanged`, `.gait.ron` is gameplay data read at runtime), the file is
read and validated again and every rig on the handle takes the new settings in the same frame's
`step_gait_rigs`. A reload that fails keeps the settings in use; `GaitAssets::error` holds its error until
the next good save.

### `anim.gait_validate`

Checks a file the way the loader does, without touching any rig.

Parameters: `path` (project-relative `.gait.ron`) or `text` (its contents, at most 1 MiB; wins over
`path`). Result: `{ path, ok, format_version, legs, patterns, errors: [{ code, message }] }`. A file that
fails is `ok: false` with one error, not an RPC error; a path outside the project is
`path_outside_project`, a missing file `F0003`, neither parameter is invalid params.

```json
{"method":"anim.gait_validate","params":{"path":"assets/units/walker.gait.ron"}}
{"result":{"path":"assets/units/walker.gait.ron","ok":true,"format_version":1,"legs":6,"patterns":3,"errors":[]}}
```

Error codes (lowercase, also the prefix of the message):

| code | meaning |
|---|---|
| `gait_parse` | not a `.gait.ron` document: RON syntax, a missing required field, an unknown field, a wrong type, or no `format_version` |
| `gait_version` | `format_version` is 0 or newer than this build reads (checked before the typed parse) |
| `gait_legs` | the leg list's length is not the patterns' leg count, or a leg has an empty joint name |
| `gait_pattern` | the stepper's `GaitError::Pattern`: a preset with an unsupported leg count, offsets or duty out of range, more than 4 patterns, switch speeds that do not fit |
| `gait_settings` | the stepper's `GaitError::Settings`: any other setting out of its documented range (posture included) |
| `gait_io` | the file could not be read (`GaitAssets::load_file`, a reload) |

The same check runs on load and on every hot reload, so these are the codes a running game logs
(target `regia_animation`, field `code`).

Acceptance: `gait_ron_round_trips`, `gait_ron_drives_the_same_trace_as_the_literal`,
`newer_gait_version_is_refused`, `invalid_gait_ron_reports_gait_settings`,
`gait_rig_uses_a_gait_asset_handle`, `hip_offset_overrides_keep_their_value`,
`reloaded_gait_asset_applies_next_frame`, `gait_asset_rig_allocates_nothing_after_warmup` and
`the_frozen_format_is_covered` in `regia_animation --test gait_asset`;
`gait_files_are_validated_through_dispatcher` in `regia_agent --test gait_asset`; the `anim.gait` fuzz
target and the `gait/v1.ron` format corpus fixture.

Version 1 is frozen (v0.7 format batch 2, ADR-10266): a later engine reads every v1 file, and a change
to the format is a version 2 with a migrator, so `regia upgrade-project --write` brings old files
forward. Proofs: `the_corpus_covers_every_v1_shape`, `every_nested_block_refuses_an_unknown_field` and
`every_corpus_file_refuses_other_versions_and_migrates_idempotently` in `regia_animation --test
gait_v1_freeze`, over the corpus files `gait/v1.ron`, `v1-hexapod-switching.ron`, `v1-quadruped.ron` and
`v1-biped.ron`; `upgrade_project_leaves_gait_v1_files_and_skips_a_newer_one` in `regia_cli --test
format_corpus_upgrade`.

## See it: the locomotion overlay

Nothing about a walker has to be tuned blind: the `locomotion` gizmo group (ADR-8729, off by default) draws
what the stepper decided this frame for every `GaitRig`. Turn it on, then take a screenshot:

```json
debug.set_gizmos { "group": "locomotion", "enabled": true, "on_top": true }
→ { "before": { "name": "locomotion", "enabled": false, ...,
                "toggles": { "gait": true, "foot_placement": true, "ik": true } },
    "after":  { "name": "locomotion", "enabled": true, "on_top": true, ...,
                "toggles": { "gait": true, "foot_placement": true, "ik": true } } }
seeing.screenshot {}
```

With the `gait` sub-toggle (colors are theme tokens, so they follow the project's theme):

| Drawn | Color token | Reads |
|---|---|---|
| a ring per foot target | `color.success` planted, `color.warning` swinging | `GaitState.feet[i]` (`planted`, `normal`) |
| a cross per hip (neutral foothold) and a line from the hip at body height to the foot | `color.text-muted` | `GaitParams::hip_offsets` turned by the body yaw |
| each swing arc, lift-off to landing | `color.warning` | the stepper's own arc (`regia_animation::gait::swing_arc_point`), so the foot is always on it |
| the support polygon of the planted feet | `color.accent` | the hull `stability_margin` is measured against |
| a ring around the body, radius = the stability margin | `color.success` stable, `color.danger` unstable | `GaitState.stability_margin` |
| an X on a hanging foot, and on the body when `refused` grew this frame | `color.danger` | `FootMode::Hanging`, `GaitState.refused` |
| a diamond on a forced or waiting foot | `color.info` | `FootMode::Forced` / `Waiting` (`forced_steps`) |
| the body attitude frame: right, up, forward | `color.danger`, `color.success`, `color.accent` | the body joint's world rotation (else the heading) |

With the `ik` sub-toggle each leg draws its chain (bones, a cross per joint), its target (the foot target
raised by the bind tip height) with a line from the tip to it, a reach sphere around the chain root, and
for FABRIK legs with `GaitLeg::limits` a cone per ball joint and an arc per hinge (`color.accent-hover`).

Pick what to see with `toggles` (any subset; the others keep their value):

```json
debug.set_gizmos { "group": "locomotion", "toggles": { "ik": false } }
```

Errors (`invalid_params`): `toggles` that is not an object, an unknown sub-toggle name, a non-boolean
value, or `toggles` on a group other than `locomotion`. Nothing changes on an error.

The overlay's system runs only while the group is enabled, so it costs nothing when off; when on it reads
the presentation state and never writes it, and allocates nothing per frame after warm-up. A host that
steps a walker by hand (no `GaitRig`) calls `regia_framework::locomotion_gizmos::draw_gait` with its own
`GaitParams` and `GaitState`. Acceptance: `cargo test -p regia_framework --test locomotion_gizmos`
(`locomotion_gizmos_off_draws_nothing`, `on_draws_the_expected_primitive_counts_for_a_hexapod`,
`on_draws_joint_limits_for_fabrik_legs`) and `debug_set_gizmos_toggles_locomotion` in
`regia_agent --test locomotion_gizmos`.

## Worked scene: the anim_lab locomotion lab

`samples/anim_lab` has a scene to try these methods on (`anim_lab::locomotion`, ADR-8807): a `GaitRig`
hexapod in driven mode walks along −Z over three 8 cm steps and a 10° slope, stops and turns in place,
next to a `FootPlacement` biped on stairs. A scripted mover sets each rig's `Transform` (and the hexapod's
`GaitVelocity`) every frame from the frame number, and the clock advances exactly 1/60 s per frame, so the
same frames give the same answers. Each walker's `ScriptedWalker.frame` is the script frame of its pose: read it
with `scene.get_entity` to step relative to where the scene is, or wait on it with `play.wait_until`
(predicates read components; only a fixed list of resources can be a predicate target). A typical check:

<!-- not-a-request -->
```text
play.pause {}
scene.query_entities { "component": "GaitRig" }          -> the hexapod's id
play.step { "frames": 405 }                               -> on the steps
play.expect { "assertions": [ { "name": "no forced steps", "condition":
    { "entity": <id>, "component": "GaitState", "path": "forced_steps", "op": "eq", "value": 0 } } ] }
anim.locomotion_state { "entity": <id> }                  -> gait.planted_count >= 3, attitude.pitch on the slope
```

Acceptance: `cargo test -p anim_lab --test locomotion_acceptance` (`locomotion_scene_acceptance`: no
planted foot slides more than 0.1 mm, no foot target moves farther in a frame than the swing bound
`anim.gait_simulate` reports for these settings, no forced step or
replant, a tripod always down, leg tips within 1 mm of their targets) and `--test locomotion_agent`
(`locomotion_agent_dogfood`, and `locomotion_agent_screenshot` where a Vulkan device exists). The user
guide's "Legged units: a worked example" walks through the code.
