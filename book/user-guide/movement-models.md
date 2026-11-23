# Movement models

Regia has two kinds of movement code, and a game picks per model which one it uses (ADR-8841):

- **Lockstep models** in `regia_detsim` (and the collision queries of `regia_world_geom`): pure steps that give
  the same bits on every CPU architecture, checked by `cargo xtask det-scan` and pinned by determinism goldens.
- **Physical models** in `regia_physics` (rapier): rigid bodies, joints, tires, ragdolls. They give the same bits
  on the same build and the same CPU architecture, which is what a server, a replay or single-player needs.

A lockstep game runs its simulation on every machine from inputs only, so every model on that simulation must be
a lockstep model. A server-authoritative or single-player game may use either kind.

## The rule for lockstep games

- The simulation reads only the steps and queries of the lockstep closure (`docs/ci/lockstep-closure.toml`):
  `regia_detsim`, `regia_world_geom`, `regia_nav::det`, `regia_rules` and `regia_net::lockstep`, plus the game's
  own code under the same scan.
- Physics output never feeds back. The simulation reads no rapier pose, velocity, contact, query hit or event, no
  `Transform` that physics writes, and no state of an ECS adapter.
- Physics may still run beside the simulation, one way: the simulation's snapshot and events drive kinematic hulls,
  debris and ragdolls ([Presentation physics](deterministic-simulation.md#presentation-physics)). Two players may
  see different debris; that is presentation, not a desync.
- The ECS adapters (for example `regia_framework::det_hover`) are not on the closure. A lockstep simulation calls
  `regia_detsim` directly.

## What each guarantee means

| Guarantee | What it means | Who relies on it |
|---|---|---|
| Cross-arch bits | The same inputs give the same bits on x86-64 and aarch64 (Linux, Windows, macOS, Android). The code is det-scanned and each model has a golden; the targets a golden has been recorded on are listed in its `.detgold` file. | Lockstep games |
| Same build and arch | The same build, on the same CPU architecture, with the same inputs gives the same bits. Two architectures may differ from the first steps. | Servers, replays, client prediction against a server, single-player |
| None | Each client computes its own result; nobody compares it. | Presentation only |

## The models

| Model | Crate and module | Guarantee | Agent methods | Page |
|---|---|---|---|---|
| Hover hull (lockstep) | `regia_detsim::hover`, its host kit `hover::host` | Cross-arch bits (goldens `hover_libm_1000`, `hover_mixed`, `hover_fleet_terrain`) | none: it runs in the game's own simulation | [Hover step (lockstep)](../agents/det-hover.md) |
| Hover hull in an ECS app | `regia_framework::det_hover` | Same build and arch (the step inside is the lockstep one; the ECS glue is not scanned) | `detsim.hover_list`, `detsim.hover_get`, `detsim.hover_step`, `detsim.hover_set_input`, `detsim.hover_probe`, `detsim.hover_hull_check` | [Hover hulls in an ECS app](../agents/det-hover.md#hover-hulls-in-an-ecs-app) |
| Hover craft (physical) | `regia_physics::hover` | Same build and arch | `physics.hovercraft`, `physics.vehicle_input` | [Hover craft](../agents/vehicles.md#hover-craft) |
| Walker (lockstep) | **planned**: the walker step (walk, run, crouch, jump windows, suit flight) is brief movement-physics-09 (lane `buildout-mp-det-walker`), not on the v0.7 line yet. Available now: the collision side, `regia_world_geom::mover::{move_disc, move_capsule}` | Cross-arch bits (the mover) | none | [Character mover](../agents/world-geometry-queries.md#character-mover) |
| Character (physical) | `regia_framework::character` over `regia_physics::character`; online prediction in `regia_framework::net_character` | Same build and arch | `physics.characters` | [Character physics](../agents/character-physics.md), [Net characters](../agents/net-character.md) |
| Ground vehicles (lockstep, kinematic) | `regia_detsim::ground_vehicle::{tracked, wheeled}` | Cross-arch bits (golden `ground_vehicle_mix`) | `detsim.ground_vehicle_simulate`, `physics.track_step` | [Ground vehicles (lockstep)](../agents/det-ground-vehicles.md) |
| Vehicles (physical: tires, drivetrain, suspension) | `regia_physics::vehicle`; online in `regia_framework::net_vehicle` | Same build and arch | `physics.vehicles`, `physics.vehicle_input`, `physics.vehicle_reset`, `physics.vehicle_telemetry` | [Vehicles (agents)](../agents/vehicles.md), [Net vehicles](../agents/net-vehicles.md) |
| Projectiles and aim | `regia_detsim::projectile`, `regia_detsim::aim` | Cross-arch bits (golden `projectile_mix`) | `detsim.projectile_simulate`, `detsim.aim_solve` | [Lockstep projectiles and aim](../agents/det-projectiles.md) |
| Bodies that push each other | `regia_detsim::contact` over `regia_detsim::broadphase` | Cross-arch bits (golden `contact_pile`) | none: the game reports `ContactStats` in its own rows; `sim.det_debug_draw` shows the index | [Contact response](../agents/det-ids-broadphase.md#contact-response) |
| World gravity, air, vacuum, low gravity | `regia_detsim::site_physics::WorldPhysics`, `regia_detsim::lowg` | Cross-arch bits (golden `lowg_ballistic_mixed`) | none | [Model limits and worlds](../agents/det-hover.md#model-limits-and-worlds) |
| Rigid bodies: stacks, joints, ragdolls, cloth, destruction | `regia_physics` | Same build and arch (on a lockstep simulation: not promised, see below) | `physics.stats`, `physics.raycast` | [Physics (agents)](../agents/physics.md) |
| Presentation physics beside a simulation | `regia_framework::sim_bridge::presentation` | None, by design | `sim_bridge.presentation_stats` | [Presentation physics (agents)](../agents/presentation-physics.md) |
| Pixel platformer (2D) | `regia_physics::platformer2d` | Same build and arch (no golden across architectures) | none | [2D: Platformer physics](2d.md#platformer-physics) |

## By genre

| Genre | Network model | Movement to use | Guarantee you get |
|---|---|---|---|
| Strategy and tactics (hundreds of units) | Lockstep | `regia_detsim::ground_vehicle` or `regia_detsim::hover` for units, `regia_detsim::contact` so units push each other, `regia_detsim::projectile` for shots, `regia_nav::det` for paths. Wrecks and debris through presentation physics. | Cross-arch bits |
| Arena shooter | Server-authoritative (recommended) | The physical character (`regia_framework::character`) with `net_character` prediction and lag compensation; `regia_physics` queries for hits; ragdolls anywhere. | Same build and arch on the server |
| Arena shooter | Lockstep (small matches) | The walker step is **planned** (movement-physics-09). Until it lands: `regia_world_geom::mover::move_capsule` with your own intent step in your simulation, and `regia_detsim::projectile`. | Cross-arch bits for what you build on the closure |
| Vehicle combat (hover tanks, armed cars) | Lockstep | `regia_detsim::hover` or `regia_detsim::ground_vehicle`, `regia_detsim::projectile`, `regia_detsim::contact`; explosions and debris through presentation physics. | Cross-arch bits |
| Vehicle combat | Server-authoritative | `regia_physics::hover` or `regia_physics::vehicle` on the server, `net_vehicle` prediction on the owning client. | Same build and arch on the server |
| Platformer | Single-player or local multiplayer | 2D: `regia_physics::platformer2d`. 3D: `regia_framework::character`. | Same build and arch (enough for replays on one machine) |
| Racing | Single-player or server-authoritative | Physical handling: `regia_physics::vehicle` (tires, drivetrain), `net_vehicle` online. | Same build and arch |
| Racing | Lockstep (arcade handling) | `regia_detsim::ground_vehicle::wheeled` (a kinematic bicycle model, no tires or gears). The rapier ray-cast car on a lockstep simulation is measured but not promised (below). | Cross-arch bits |
| Co-op shooter with server authority | Server-authoritative | Physical characters and vehicles on the server, `net_character` and `net_vehicle` on the clients, ragdolls and debris anywhere. A lockstep step (hover, ground vehicle) is still the best model to predict with, because a rollback replays a pure function. | Same build and arch on the server |

## Choosing, step by step

```text
Does every machine run the whole simulation from the players' inputs only (lockstep)?
|
+-- yes: the simulation calls only the lockstep closure.
|   +-- hover hull ................ regia_detsim::hover (+ hover::host for terrain and fleets)
|   +-- tank or car ............... regia_detsim::ground_vehicle (tracked or wheeled; kinematic)
|   +-- person on foot ............ regia_world_geom::mover now; the walker step is planned (movement-physics-09)
|   +-- shell, grenade, bullet .... regia_detsim::projectile (+ regia_detsim::aim)
|   +-- bodies bump each other .... regia_detsim::contact
|   +-- gravity, air, low gravity . regia_detsim::site_physics::WorldPhysics (in the settings handshake)
|   +-- stacks, joints, ragdolls,
|       debris, cloth ............. regia_physics, presentation only; the simulation never reads it back
|
+-- no: a server owns the truth, or the game is single-player.
    +-- physical handling, ragdolls, stacks ... regia_physics on the server (or locally)
    +-- predict your own character or vehicle . regia_framework::net_character / net_vehicle
    +-- a pure model to replay in a rollback .. a regia_detsim step works here too
```

## Rigid bodies on a lockstep simulation: not promised

Rapier gives the same bits across CPU architectures only with `regia_physics`' `physics-cross-platform-determinism`
feature, and only for what has been measured. Brief movement-physics-12 (ADR-8511, landing on the v0.7 line) ran
five rapier scenes for 1200 steps each (a 50-box stack, boxes and capsules sliding down a ramp, a chain of revolute
joints, the character controller over a course, the ray-cast car over a bump strip):

- with the feature: every scene equal per step on x86-64 Linux, aarch64 macOS and x86-64 Windows;
- without the feature: every scene differs between x86-64 and aarch64 within the first 22 steps;
- not measured yet: Android and the Steam Deck; no scene covers multibody joints, continuous collision detection,
  rapier's `parallel` feature, other shapes or other joint kinds.

Until the owner rules (by 2026-12-01), Regia **does not promise** rigid bodies on a lockstep simulation: rapier is
presentation-only in a lockstep game. A game that still puts the measured subset on its simulation does so outside
that promise: it turns the feature on for every player (the feature changes the bits on one machine too, so put the
setting in your content handshake), stays within the measured shapes and joints, and runs its own determinism check
on every target it ships. The feature also switches the whole build to glam's scalar math (the renderer included),
which is slower.

## Planned, not available

- The lockstep walker step (movement-physics-09, lane `buildout-mp-det-walker`).
- Hover client prediction in the netcode (net-sota 1306).
- A feel-parity test between the physical and the lockstep hover.
- Lockstep aircraft beyond suit flight; gravity directions, gravity volumes and spherical gravity.
