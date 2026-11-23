# Continuous collision detection (CCD queries)

`regia_physics::ccd` answers one question without a physics world: two convex shapes
move from pose A to pose B over one step — do they touch, and if so, when and where?
Use it for projectiles and other fast movers that a discrete overlap test would let
tunnel through thin geometry, for kinematic sweeps, and for placement or probe checks
in tools. For simulated bodies, rapier's own CCD (`RigidBody::ccd`) still applies,
and the opt-in `SweptCcd` component runs this module's angular sweep inside the step
(see "Swept CCD in the step" below).

## The query

Shapes are `ConvexInput` values (point, sphere, box, capsule, cylinder, cone, segment,
or a convex hull from points). `ConvexInput::from_collider_shape` converts the convex
`ColliderShape` variants; `TriMesh`, `HeightField` and `Compound` return `None` (query
a compound's convex children one by one). Poses are `regia_core::math::Isometry`
(rotation + translation, no scale — bake scale into the shape sizes).

```rust,wrap
use regia_core::math::{Isometry, Vec3};
use regia_physics::ccd::{time_of_impact, ConvexInput, ToiResult};

// A 10 cm sphere flying 10 m in one step (500 m/s at 50 Hz) at a 1 cm wall.
let ball = ConvexInput::sphere(0.1);
let wall = ConvexInput::cuboid(Vec3::new(0.005, 2.0, 2.0));
let result = time_of_impact(
    &ball,
    Isometry::from_translation(Vec3::new(-5.0, 0.0, 0.0)),
    Isometry::from_translation(Vec3::new(5.0, 0.0, 0.0)),
    &wall,
    Isometry::IDENTITY,
    Isometry::IDENTITY,
    0.0,   // target_gap: stop at surface contact
    1e-4,  // tolerance on the separation at the reported t, metres
    64,    // max conservative-advancement iterations
);
match result {
    ToiResult::Hit { t, normal, point } => {
        // t is normalised to the step: impact time is t * dt.
        // normal points from shape A (the ball) toward shape B (the wall);
        // point is the witness point on shape A.
    }
    ToiResult::Separated => { /* never within target_gap this step */ }
    ToiResult::OverlappedAtStart => { /* already interpenetrating at t = 0 */ }
    ToiResult::Failed(reason) => { /* bad input or iteration budget; see ToiFailure */ }
}
```

Translation is interpolated linearly and rotation by slerp between the endpoint
poses. Rotation is accounted for conservatively (its swept angle times the shape's
bounding radius bounds how fast the gap can close), so spinning shapes cannot tunnel
either. `time_of_impact_angular` is the same query with caller-supplied swept-angle
bounds per shape (for example `|angular_velocity| * dt`); a larger bound only makes
the steps smaller, never the answer wrong. It does not model multi-turn spins whose
intermediate orientations leave the slerp path — subdivide the step for those.

`target_gap` is a skin width: the sweep stops when the shapes are that far apart.
`tolerance` is how accurate that stopping separation must be, in metres. Touching, or
already being within `target_gap`, at `t = 0` is a `Hit` with `t = 0`; only actual
interpenetration is `OverlappedAtStart`. Degenerate shapes (zero radius, zero
half-extent, points, segments) are legal. No input panics: NaN or infinite numbers,
negative sizes, empty hulls, zero quaternions, negative gap/tolerance and a zero
iteration budget all return `Failed` with a `ToiFailure` reason.

Two helpers cover the common cases: `sphere_sweep` (a sphere between two centres
against a sweeping convex shape) and `swept_box_vs_thin_wall` (a moving box against a
static box wall). `convex_distance` exposes the underlying GJK distance query
(distance, witness points, normal, overlap flag) at fixed poses — handy for proximity
checks and for verifying a TOI result.

## Choosing parameters and costs

32–128 iterations cover ordinary gameplay sweeps. Grazing near-misses with a very
small tolerance may need more and return `Failed(MaxIterations)` rather than guess;
raise the budget or the tolerance, or subdivide the step. The queries make a few small
heap allocations per distance query (the working simplex), run no broad phase, and are
deterministic: same inputs, bit-identical outputs on one platform (fixed iteration order,
no hash maps, no clock).

`convex_distance` is accurate to about 1e-6 m when its iteration converges; on curved
shapes f32 noise sometimes stops it first, and it then reports a proven lower bound (at
most a few 1e-4 m low, never high), which keeps the time of impact conservative.

## How it works

Distance is Gilbert–Johnson–Keerthi (GJK) on the Minkowski difference of the two
shapes. Time of impact is conservative advancement: each step advances time by
`(separation − target_gap) / closing-speed-bound` along the current witness normal,
which provably cannot jump past the target separation. Papers and provenance are
recorded in `docs/provenance.md` under "Physics / CCD".

## Rigid motions: the true angular sweep

`time_of_impact_motion` sweeps two shapes along constant-velocity rigid motions instead
of between two endpoint poses. A `RigidMotion` is a start pose, a rotation centre in the
shape's local frame (usually the centre of mass), the centre's displacement over the
interval, and a rotation vector (world axis times radians, any size). That is the motion
a physics step integrates, so a body that turns more than half a turn per step, or turns
about a centre other than its pose origin, is swept along its real path. An endpoint
slerp cannot see either: three whole turns have identical endpoint poses.

```rust,wrap
use regia_core::math::{Isometry, Vec3};
use regia_physics::ccd::{time_of_impact_motion, ConvexInput, RigidMotion, ToiOptions, ToiResult};

// A 1 m bar spinning 30 rad/s about its centre, thrown at 120 m/s, over one 60 Hz step.
let bar = ConvexInput::cuboid(Vec3::new(0.5, 0.02, 0.02));
let motion = RigidMotion::from_velocities(
    Isometry::from_translation(Vec3::new(4.0, 0.0, 0.0)),
    Vec3::ZERO,                    // rotation centre, local
    Vec3::new(120.0, 0.0, 0.0),    // m/s
    Vec3::new(0.0, 0.0, 30.0),     // rad/s, world
    1.0 / 60.0,
);
let wall = RigidMotion::stationary(Isometry::from_translation(Vec3::new(5.0, 0.0, 0.0)));
let result = time_of_impact_motion(
    &bar, &motion,
    &ConvexInput::cuboid(Vec3::new(0.005, 3.0, 3.0)), &wall,
    ToiOptions::default(),        // gap 0, tolerance 0.1 mm, 128 advances
);
if let ToiResult::Hit { t, .. } = result {
    let pose_at_impact = motion.pose_at(t);
}
```

The advance bound is directional: along the fixed witness normal `n`, a point at radius
`r` from a centre spinning at `ω` moves at most `|n × ω| r` toward the other shape. A spin
about the contact normal therefore costs no extra steps; a spin across it costs
`|ω| R` like the endpoint query.

Refusals, never loops. NaN or infinite numbers anywhere return
`Failed(NonFiniteInput)`. A closing-speed bound that is not finite (velocities whose
difference overflows f32, or an angular speed whose bound overflows) or so large that an
advance no longer moves `t` returns `Failed(DegenerateMotion)`. Otherwise the work is
bounded by `max_iterations` advances plus at most 48 bisection steps. A zero closing
speed is a proof that the shapes stay apart, so it returns `Separated`.
`ToiFailure::code()` gives the stable lowercase code (`non_finite_input`,
`degenerate_motion`, ...). The endpoint query `time_of_impact` now refuses overflowing
motion the same way: it used to read a NaN closing speed as "no motion".

## Swept CCD in the step

Add `SweptCcd` to a dynamic body to sweep its whole step motion, rotation included,
against the colliders near its path. If something is reached first, the body goes back
to the time of impact, `skin` short of contact, and an inelastic contact impulse at the
impact point (with the pair's average restitution) removes the closing velocity. The
solver's speculative contacts hold the contact from the next step, so the rest of the
step's motion is dropped.

```rust,wrap
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, RigidBody, SweptCcd, Velocity};
use regia_scene::Transform;
# use regia_ecs::prelude::Commands;
fn spawn_fast_body(mut commands: Commands) {
commands.spawn((
    Transform::from_translation(Vec3::new(0.0, 1.5, 0.0)),
    RigidBody::dynamic(),
    Collider { shape: ColliderShape::Box { half_extents: Vec3::new(1.0, 0.02, 0.02) }, ..Default::default() },
    Velocity { linear: Vec3::new(40.0, 0.0, 0.0), angular: Vec3::new(0.0, 0.0, 70.0) },
    SweptCcd::default(),          // skin 5 mm, 64 advances per pair
));
}
```

- It is opt-in and off by default. Without any `SweptCcd` body the step does no extra
  work, and the simulation is bit-identical to a build without the hook. A `SweptCcd`
  body that never hits anything also leaves the world bit-identical.
- `hits`, `failures` and `last_toi` on the component report what the step did.
- It sweeps against every non-sensor collider whose collision and solver groups interact
  with the body. A moving body is swept along its own step motion (exactly, when it is a
  `SweptCcd` body too; otherwise traced back from its pose and velocity), so two fast
  bodies fired at each other meet instead of passing through. Fixed and resting bodies
  are held where they are. Spheres, boxes, capsules, cylinders, cones,
  convex hulls, triangles, segments and compounds of them work on both sides. Triangle
  meshes and heightfields work as the other side: at most 256 of their triangles inside
  the swept bounds per pair. Rounded shapes, half-spaces, polylines and voxels are skipped.
- A pair already within the solver's prediction distance (2 cm by default) at the start
  of the step is left to the solver, which holds that contact. The exception is a body
  part that swings deeper than that distance through the starting contact plane, like
  the far end of a bar pivoting on its tip into a wall. The first such crossing, when it
  is a real overlap, counts as the impact, up to that depth inside the other shape (the
  solver pushes it out).
- What rapier already does: with rapier 0.36 every fast body is swept against *fixed*
  colliders, and bullets (`RigidBody::ccd`) against all bodies except other bullets.
  `SweptCcd` matters for thin *kinematic* or *dynamic* geometry (doors, platforms,
  vehicle panels, debris) hit by bodies that are not bullets, and for two bullets.
  Rapier also caps one step's rotation at an eighth of a turn (pi/4).
- The impulse acts on the swept body only. The other body is treated as immovable for
  that impulse, and the solver resolves both from the next step.
- It is independent of `RigidBody::ccd`, rapier's own CCD. A body may use both.
- Cost: one sweep per nearby pair, from about a microsecond (boxes, translation) to tens
  of microseconds (fast spins against hulls). Use it for a few fast movers (projectiles, thrown props, debris
  shards), not for every body in a pile.

The agent method `physics.time_of_impact` runs the same sweep on demand
(`book/agents/physics-time-of-impact.md`).
