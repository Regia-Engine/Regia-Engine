# Vehicles

<!-- index: Vehicle -->
<!-- index: Wheel -->
<!-- index: Drivetrain -->
<!-- index: Trailer -->
<!-- index: Aircraft -->
<!-- index: Hover craft -->
<!-- index: Bike -->
<!-- index: Tracked vehicle -->

`regia_physics::vehicle` drives wheeled vehicles with rapier's ray-cast vehicle controller. A
vehicle is one dynamic chassis body. Each wheel is a ray cast down its suspension each step, not a
rigid body. The ray finds the ground, a spring and damper push the chassis up, and tire impulses
drive, brake and corner it. This is cheap and stable at game speeds. It does not model tire
deformation. Wheel spin, a gearbox, and differentials are the optional geared drivetrain below.
Trailers, a two-wheel bike, a hover hull, a fixed-wing aircraft, and skid-steer tracks are
separate models under [Other hulls](#other-hulls). The editor's
[Vehicle Tuning](editor-guide/vehicle-tuning.md) tab edits the car, trailer, aircraft, bike,
and hover hull. Agent calls are
[Vehicles (agents)](../agents/vehicles.md).

## Axes and units

The engine's axes apply: the chassis' forward is local −Z, right is +X, up is +Y. Every wheel's
suspension points down the chassis' −Y and its axle runs along +X, so model wheel meshes with the
axle on +X. Distances are meters, mass kilograms, forces newtons, angles radians.

## Configure

`VehicleConfig::default()` is a 1,200 kg four-wheel car: a 1.8 m × 0.7 m × 4.2 m box, 2.6 m
wheelbase, 1.6 m track, front-wheel steering, rear-wheel drive and brakes on all four wheels.
Wheels 0 and 1 are front left and right; 2 and 3 are rear left and right.

```rust
use regia_core::math::Vec3;
use regia_physics::vehicle::{VehicleConfig, VehicleError, WheelConfig};

fn all_wheel_drive_car() -> Result<VehicleConfig, VehicleError> {
    let mut config = VehicleConfig::default();
    config.max_engine_force = 4500.0; // per driven wheel at full throttle
    for wheel in &mut config.wheels {
        wheel.drive = true; // all-wheel drive
    }
    config.wheels.push(WheelConfig {
        position: Vec3::new(0.0, -0.2, 2.0), // a trailing fifth wheel
        ..WheelConfig::default()
    });
    config.validate()?;
    Ok(config)
}
```

| `VehicleConfig` field | Meaning |
|---|---|
| `chassis_half_extents`, `chassis_mass`, `center_of_mass` | The chassis box, its mass and its center of mass in chassis space. Lower the center of mass to keep the car upright in hard turns. |
| `max_engine_force` | Newtons per driven wheel at full throttle. |
| `max_brake_force` | Newtons per braking wheel at full brake. |
| `rolling_resistance` | Newtons per wheel that is neither driving nor braking, so a coasting car slows down. 0 lets it roll forever. |
| `max_steer_angle` | Steered wheels' angle at full lock, in `[0, π/2)`. |
| `wheels` | 1 to 32 `WheelConfig`s. Their order is the readout order. |

| `WheelConfig` field | Meaning |
|---|---|
| `position` | The suspension's top (hard point) in chassis space. It may sit inside the chassis collider: wheel rays ignore the chassis. |
| `radius` | Wheel radius. |
| `suspension_rest_length`, `suspension_travel` | Spring length at rest and how far it may compress or extend from it. The ray reaches `rest length + radius` below the hard point. |
| `suspension_stiffness` | Spring rate **per kilogram of chassis** (1/s²): a wheel pushes with `stiffness · compression · chassis_mass` newtons. `n` equal wheels settle compressed by about `g / (n · stiffness)`. The default 30 compresses each of four wheels by about 8 cm. |
| `suspension_compression_damping`, `suspension_relaxation_damping` | Damping while compressing and extending, per kilogram of chassis. A damping ratio of about 0.3 to 0.6 is `ratio · 2 · sqrt(n · stiffness) / n`. Raise relaxation damping if the car bounces. |
| `max_suspension_force` | Cap on one suspension's force. |
| `friction_slip` | Tire grip: the most tire impulse per step is `suspension force · dt · friction_slip`. Higher values brake and corner harder, and can flip the car. |
| `side_friction_stiffness` | Multiplier on the cornering impulse. |
| `steer`, `drive`, `brake` | Whether the wheel steers, is driven, and brakes. |

`validate` names the first bad field: `VehicleError::InvalidWheel { index, field }`,
`InvalidConfig { field }`, `NoWheels` or `TooManyWheels`. `VehicleError::code()` gives a stable
lowercase identifier (`vehicle_invalid_wheel`, …) for logs and the agent protocol.

## With the ECS

Add `VehiclePlugin` after `PhysicsPlugin` and create vehicles with `spawn_vehicle`. The entity gets
the chassis' `RigidBodyHandle` and `ColliderHandle`, so the physics step writes its `Transform` and
queries and collision events name it. It also gets a `VehicleController` and a `VehicleInput`.

```rust
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsPlugin;
use regia_physics::vehicle::{VehicleConfig, VehicleError, VehicleInput, VehiclePlugin, spawn_vehicle};

fn add_car(app: &mut App) -> Result<Entity, VehicleError> {
    app.add_plugins((PhysicsPlugin, VehiclePlugin));
    spawn_vehicle(app.world_mut(), VehicleConfig::default(), Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY)
}

/// The player's controls, filled by the game's input mapping.
#[derive(Resource, Default)]
struct Controls {
    accelerate: f32,
    reverse: f32,
    brake: f32,
    stick_x: f32,
}

fn drive(mut cars: Query<&mut VehicleInput>, controls: Res<Controls>) {
    for mut input in &mut cars {
        input.throttle = controls.accelerate - controls.reverse; // -1..1
        input.brake = controls.brake; // 0..1
        input.steer = controls.stick_x; // +1 = right
    }
}
```

The plugin updates every vehicle once per `FixedUpdate`, inside the `PhysicsStep` chain after the
velocity sync and before the step, in entity spawn order. Input is clamped; a non-finite value
reads as released. When an update fails (the chassis was removed, say), the error lands in
`VehicleController::last_error` and the vehicle is skipped. Nothing panics. Despawning the entity
removes the chassis from the world.

`throttle` and `brake` interact per wheel: on a wheel that can brake, braking cancels its engine
force. With brakes on every wheel, throttle plus brake holds the car still.

## Without the ECS

`Vehicle` works on a bare `PhysicsWorld`, for tools and tests:

```rust
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleError, VehicleInput};

fn make_car(physics: &mut PhysicsWorld) -> Result<Vehicle, VehicleError> {
    Vehicle::spawn(physics, VehicleConfig::default(), Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY)
}

/// Every fixed step, before the world steps.
fn drive_one_step(
    physics: &mut PhysicsWorld,
    vehicle: &mut Vehicle,
    input: VehicleInput,
    dt: f32,
) -> Result<(), VehicleError> {
    vehicle.update(physics, input, dt)
}
```

`Vehicle::attach` turns an existing dynamic body with mass into a chassis and keeps its colliders.
A vehicle's chassis never sleeps, because its wheels push it every step.

## Wheel poses for rendering

`vehicle.wheel_states(&physics)` returns one `WheelState` per wheel, posed against the chassis'
current pose:

- `local_translation` / `local_rotation`: the wheel in chassis space (the hard point moved down by
  the suspension length; steer about +Y, then spin about +X). Use these when wheel meshes are
  children of the chassis entity.
- `world_translation` / `world_rotation`: the same in world space.
- `steer_angle` (about +Y, a right turn reads negative) and `spin_angle` (grows while rolling
  forward; the mesh turns by `-spin_angle` about +X).
- `grounded`, `compression`, `suspension_length`, `contact_point`, `contact_normal`,
  `ground_collider`, `ground_entity` and `suspension_force` for gameplay: dust, skid marks, sounds.

`vehicle.state(&physics)` adds the chassis pose, `forward` axis, velocities, `forward_speed` and the
grounded-wheel count. Its `linear_velocity` is rapier's end-of-step velocity. A car resting on its
springs reads a small downward vertical velocity (about −0.06 m/s at 60 Hz) while its position
holds still: the suspension impulse lands once per step and gravity acts across the solver's
substeps. `forward_speed` is not affected.

## Reset

`vehicle.reset(&mut physics, position, rotation)` teleports the chassis to a pose at rest (no
velocity, no forces) and clears the wheels. `reset_to_spawn` returns to the pose it was spawned or
attached at. No wheel is grounded until the next update casts the rays.

## A geared drivetrain

`VehicleConfig::drivetrain` picks the drive model. `Drivetrain::Simple`, the default, is `max_engine_force` per driven
wheel. `Drivetrain::Geared` has an engine with a torque curve, a slip clutch, a gearbox (automatic or manual) and open,
locked or limited-slip differentials for rear, front or all-wheel drive. Each wheel has its own spin, the tire force
follows the slip ratio, and `VehicleState` reads `rpm`, `gear`, `speed` and each wheel's `slip_ratio` and `slip_angle`.

```rust
use regia_physics::vehicle::VehicleConfig;
use regia_physics::vehicle_drivetrain::{AbsConfig, AeroConfig, AntiRollBar, Differential, Drivetrain, DrivetrainConfig};

fn limited_slip_car() -> VehicleConfig {
    let mut config = VehicleConfig {
        drivetrain: Drivetrain::Geared(DrivetrainConfig {
            rear_differential: Differential::LimitedSlip { torque_bias: 2.5 },
            ..DrivetrainConfig::default()
        }),
        anti_roll: vec![AntiRollBar { left: 0, right: 1, stiffness: 14_000.0 }],
        aero: Some(AeroConfig::default()),
        ..VehicleConfig::default()
    };
    config.aids.abs = Some(AbsConfig::default());
    config
}
```

`anti_roll` and `aero` work with either model. The aids need `Geared`. Use `Vehicle::request_shift` and
`Vehicle::set_automatic_shifting` to shift in code. In `.vehicle.ron` these are the v2 sections; see
`docs/formats/vehicle.md` for every field. A v1 file loads as `Simple` and drives exactly as before.

A held brake leaves a slow creep on a slope, because a locked tire's force comes from its slip. The speed is
about `peak_slip · tan(slope) / peak_friction`, a little more at a 60 Hz step: with the default tire about 3.4 cm/s on a 15
degree hill, or 2 m a minute.

## Other hulls

These share the engine axes (forward is local −Z, right is +X, up is +Y) and they are not the
ray-cast car. None of them is a lockstep model. A lockstep game uses
`regia_detsim::ground_vehicle` for tracked and wheeled units. See
[Movement models](movement-models.md).

- **Trailer** (`regia_physics::vehicle_trailer`). A trailer chassis couples to a tow vehicle at a
  hitch, or a tractor couples at a fifth wheel. Brakes, a jackknife stop in reverse, and a
  multi-trailer train are part of the model. The asset is a trailer configuration beside the car.
- **Bike** (`regia_physics::vehicle_bike`, `.bike.ron`). Lean, steer, and speed for two wheels.
  It does not drive the car solver and it does not read a chassis body. The caller passes steer,
  speed, the drive command, and the two wheel-ray lengths each step. Below the upright speed the
  target lean is zero and a spring stands the bike up. Lean past the limit sets `fallen`.
- **Hover hull** (`regia_physics::hover`, `.hoverhull.ron`). A dynamic box held up by ray-cast
  thrusters. Each thruster pushes along the ground normal. Drive, brake, yaw, and lateral-slip
  impulses act only while some thruster has ground. `VehiclePlugin` reads the entity's
  `VehicleInput`. Forward is the same axis as the car.
- **Aircraft** (`regia_physics::aero`, `.aircraft.ron`). One dynamic body, lifting surfaces,
  engines, and landing-gear rays. Positive pitch raises the nose, positive roll lowers the right
  wing, positive yaw turns the nose right. The model has no downwash, no ground effect, no
  propeller wash, no compressibility, and no stall hysteresis. `trim` solves level flight offline.
- **Tracks** (`regia_physics::vehicle_track`). Two track speeds on one rigid body in the XZ plane.
  This is a pure function, not the wheeled solver. Throttle is clamped to −1..=1. Coasting moves
  a track toward zero and does not reverse it. Yaw rate is the speed difference divided by the
  track separation (2 m).

The proving-ground rides for these hulls are editor actions. This chapter is the runtime.

## Determinism

Same build, same platform, same inputs and fixed step: bit-identical runs (the test drives a
scripted 300-step run twice and compares `PhysicsWorld::state_digest` every step). The plugin
updates vehicles in entity spawn order, so several vehicles stay reproducible too.

Two CPU architectures (an x86-64 PC and an aarch64 phone) may compute different bits, so these
vehicles do not belong on a lockstep simulation. A lockstep game uses the kinematic tracked and
wheeled models of `regia_detsim::ground_vehicle` instead (ADR-8841; see
[Movement models](movement-models.md)).

## Agents

`physics.vehicles`, `physics.vehicle_input` and `physics.vehicle_reset` read, drive and reset
vehicles over the agent protocol, and `physics.vehicle_telemetry` and `physics.vehicle_shift` read the
drivetrain and change gear; see [Vehicles (agents)](../agents/vehicles.md).
