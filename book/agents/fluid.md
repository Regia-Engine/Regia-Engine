# Particle liquid (agents)

`regia_fluid` simulates liquid with particles: a PIC/FLIP solver on a MAC grid (ADR-2441). The CPU
reference solver is deterministic: the same inputs give bit-identical particles, which the state
digest shows. Water bodies (lakes, rivers, the sea) stay the surface shader of `regia_terrain`; use
this crate for liquid that moves as a volume, such as pours, splashes, dam breaks and streams.

Turn it on with the `fluid` feature of `regia` and add `regia::fluid::FluidPlugin` to the app. The
plugin steps every simulation set to run on `FixedUpdate`. With the renderer, it also publishes the
liquid surface (a marching-cubes mesh of the live simulation) and the whitewater points as
`MeshRenderer` entities. While `fluid.play` is showing a cache frame, that surface entity is an empty
mesh and the frame's liquid positions are not drawn. Whitewater points from the cache are drawn as
octahedra. Nothing is installed by default, so default schedules do not change.

## Methods

| Method | Params | Answers |
|---|---|---|
| `fluid.create` | `preset?` (`pour`, `dam_break`, `splash`, `stream`), `bounds?` `{ min, max }`, `cell_size?`, `particle_cap?`, `seed?`, `whitewater?` | `{ id, preset, cells, cell_size, particle_cap }` |
| `fluid.add_emitter` | `id`, `kind?` (`fill`, `source`, `sink`), `shape?` (`box`, `sphere`), `center`, `half_extents?`, `radius?`, `velocity?` | `{ id, kind, index }` |
| `fluid.step` | `id`, `steps?` (0 to 600, default 1), `run?` | the live measures (`steps` through `wetted_faces`, plus `dropped_non_finite` and `digest`) and `running`. Not `playing`, `cache_frames`, `emitters` or `sinks` (those are only on `fluid.stats`) |
| `fluid.stats` | `id` | `{ steps, particles, volume, nominal_volume, volume_drift, divergence, pcg_iterations, pcg_residual, pcg_converged, substeps, liquid_cells, foam, spray, bubbles, whitewater_generated, moving_solids, wetted_faces, digest, running, playing, cache_frames, emitters, sinks }` |
| `fluid.bake` | `id`, `steps` (1 to 600), `whitewater?` (default true), `path?` | `{ frames, bytes, whitewater, written, digest }` |
| `fluid.play` | `id`, `frame?`, `path?`, `stop?` | while a frame is showing: `{ id, playing, frames, step, points, liquid, foam, spray, bubbles }`. `stop: true` answers only `{ id, playing: null }` |
| `fluid.destroy` | `id` | `{ id, destroyed }` |
| `fluid.set_solids` | `id`, `solids` (at most 64; empty clears) | `{ id, solids }` |
| `fluid.solids` | `id` | `{ steps, solids, coupled_bodies, coupling_installed, coupling_steps, skipped_bodies }` |

- `bounds` is required without a preset. A preset lays itself out in fractions of its bounds, so
  other bounds scale it. With no bounds, each preset has its own (for example `dam_break`: 1.6 x 0.6 x
  0.1 m in 2.5 cm cells).
- The grid has at most 64³ cells (32³ on Android). The particle cap is at most 2,000,000 (65,536 on
  Android).
- `fluid.step` and `fluid.bake` refuse more than `steps * (cells + particles) = 60,000,000` in one
  call (`fluid_step_budget`). Step a big simulation in smaller batches.
- `fill` seeds the shape once at the next step. `source` keeps the cells inside topped up and sets the
  liquid there to its velocity. `sink` removes the liquid inside every step.
- `fluid.bake` records the current frame, then `steps` more frames, into a `.fluidcache` kept on the
  simulation. With `path` (project-relative, ending in `.fluidcache`), it also writes the file
  atomically. `fluid.play` selects a cache frame and no solver runs. While it plays, the drawn surface
  is built from that frame's liquid. `path` loads a cache file first. `stop: true` clears playback and
  returns to the live surface. It does not require a baked cache and returns only
  `{ id, playing: null }`. A `path` given with `stop` is still read (and adopted) first, so an
  unreadable file is `fluid_no_cache`; `frame` is still type-checked (`fluid_bad_param`), but its range
  is not. `fluid.step` with `steps > 0` also stops playback. `fluid.step` with `steps: 0` leaves
  playback as it is and still omits `playing` from its answer.
- `digest` is the state digest as 16 hex digits. Two runs with equal inputs answer equal digests at
  every step. The digest covers the particles, the whitewater, the moving solids and their
  impulses, the solver settings, the emitters, the sinks and the colliders: equal digests mean
  the states are bit-identical and the next step reads the same inputs (ADR-4753).

## Measures

- **MAC sampling:** the `u`, `v` and `w` components are sampled at their own staggered face-center
  offsets, then combined into a velocity at a local grid coordinate. Trilinear weights clamp to the
  nearest valid face range; duplicate boundary indices represent one sample and their weights sum
  to one. This keeps constant fields constant at boundaries and on one-cell axes.

- `volume`: the liquid region the projection sees, in m³. An interior liquid cell counts as one whole
  cell even when it holds fewer than 8 particles. A cell is interior when every face is closed (a
  domain-boundary face counts as closed) or opens onto another liquid cell. Any other liquid cell
  counts `min(1, n / 8)`. Empty cells count nothing. The sum times `dx³` is `volume`. `nominal_volume`:
  `particles / 8 * dx³`. `volume_drift = volume / nominal_volume - 1` is negative when particles
  clump (the volume loss of FLIP). A position correction keeps it small. An agent that recomputes
  `sum(min(1, n / 8)) * dx³` over every liquid cell will not match `volume` wherever an interior cell
  holds fewer than 8 particles.
- `divergence`: the largest weighted divergence of a liquid cell after the last pressure projection,
  in 1/s.
- `pcg_iterations`, `pcg_residual`, `pcg_converged`: the last projection's conjugate-gradient solve.
  It is capped at 200 iterations by default and stops at a relative residual of 1e-6.

## Whitewater

Foam, spray and bubble particles come from the liquid's trapped-air, wave-crest and kinetic-energy
potentials. The type comes from the number of liquid neighbors around the particle: spray is
ballistic, foam rides the surface and dissolves over its lifetime, and bubbles rise. Each type has a
cap (`max_foam` 20,000, `max_spray` 20,000, `max_bubbles` 10,000 by default), and a type never goes
over its cap. A calm pool makes none. `whitewater: false` on `fluid.create` turns it off.

## Verify a change

```text
fluid.create { "preset": "dam_break" }                -> { "id": 1, "cells": [64, 24, 4], ... }
fluid.step   { "id": 1, "steps": 60 }                 -> { "particles": 8192, "volume_drift": -0.01, "foam": 120, ... }
fluid.bake   { "id": 1, "steps": 120, "path": "fluid/dam.fluidcache" }
fluid.play   { "id": 1, "frame": 60 }                 -> { "liquid": 8192, "foam": ..., "step": 120 }
```

Run the same create and step twice and compare `digest`. To check that a solver change keeps the
liquid incompressible, read `divergence` and `volume_drift` after a fixed number of steps.

## Error codes

Every error has `data.code` `F0002` (bad parameters) or `F0003` (no such simulation or cache), with
a lowercase `data.reason`:

| Reason | When |
|---|---|
| `fluid_not_found` | No simulation with that `id`. |
| `fluid_bad_param` | A parameter has the wrong type or range (a NaN, a negative cap, an unknown `kind` or `shape`). |
| `fluid_bad_bounds` | `bounds` is missing without a preset, not `{ min, max }`, not finite, or `max <= min` on an axis. |
| `fluid_bad_cell_size` | `cell_size` is not positive. |
| `fluid_too_many_cells` | The bounds and cell size give more than 64³ cells (32³ on Android). |
| `fluid_too_many_shapes` | More than 64 emitters, sinks, colliders or moving solids. |
| `fluid_bad_solid` | A moving solid is not an object, has an unknown `shape` or `motion`, a size outside [0, 1000] m, a speed over 1000 (m/s or rad/s), or a non-finite field. |
| `fluid_too_many_sims` | More than 16 simulations in the world. |
| `fluid_unknown_preset` | `preset` is not `pour`, `dam_break`, `splash` or `stream`. |
| `fluid_step_budget` | `steps * (cells + particles)` is over one call's budget. |
| `fluid_no_cache` | `fluid.play` with nothing baked, or a `path` that cannot be read. With `stop: true` only the unreadable `path` case is raised. |
| `fluid_frame_out_of_range` | `frame` is past the cache's last frame. Not raised when `stop` is true, even if `frame` is also set. |
| `fluid_write_failed` | `fluid.bake` could not write `path`. |
| `fluidcache_truncated` | The `.fluidcache` ends early, or claims more points than its bytes can hold. |
| `fluidcache_bad_magic` | The file is not a `.fluidcache`. |
| `fluidcache_bad_version` | A `format_version` this build does not read. |
| `fluidcache_too_large` | A count over a cap (100,000 frames, 4,000,000 points per frame, 1 GiB files). |
| `fluidcache_corrupt` | Inconsistent data (bad flags, tags or key frames, a point outside the quantization box, bytes left over). |
| `fluidcache_no_frame` | A frame index past the end (library API). |
| `path_outside_project` | `path` is absolute or leaves the project (see [safety](safety.md)). |

## Moving solids and rigid-body coupling

A moving solid is a box, sphere or capsule with a pose and a linear and angular velocity (ADR-2577).
Every substep the solver folds it into the solid field at its pose for that moment. Each grid face it
closes, fully or partly, takes its velocity as the boundary condition of the pressure projection
(Batty, Bertails and Bridson 2007). The liquid moves with a moving wall and cannot flow through it,
and particles that end up inside are pushed out relative to the solid's velocity. A solid keeps moving
at its velocity until it is set again. The step's substep count covers the solids too: a solid's
fastest point (`|v| + |w| r`, `r` its bounding radius) moves at most `cfl` cells per substep, up to
`max_substeps`, so a fast thin wall does not jump over the liquid.

- `fluid.set_solids { id, solids: [...] }` sets them directly. Each solid is `{ shape?` (`box`,
  `sphere`, `capsule` along local y), `center`, `half_extents?`, `radius?`, `half_height?`,
  `rotation?` (quaternion `[x, y, z, w]`), `velocity?`, `angular_velocity?`, `motion?` (`kinematic`,
  `dynamic`), `tag? }`. An empty list clears them, and the static field comes back bit for bit.
  Setting the solids drops the impulses of the last step; the next step reports new ones.
- `fluid.solids { id }` lists them with the impulse the liquid's pressure gave each in the last step:
  `impulse` (N s), `angular_impulse` (about the solid's centre at the start of the step) and
  `wetted_faces`. It also lists the physics bodies coupled to this simulation (`coupled_bodies`; there
  is no `fluid.coupling` method).

Two-way coupling uses the `physics` feature of `regia_fluid`, which `regia/fluid` turns on. Add
`regia::fluid::coupling::FluidPhysicsPlugin` after `FluidPlugin` and `PhysicsPlugin`, then put a
`FluidBody { fluid: id, two_way, max_impulse }` on a body that has a box, sphere or capsule collider.
Every `FixedUpdate`, the plugin reads the body's pose and velocity before the fluid steps. Kinematic
bodies take their velocity from the change of their `Transform`. The turn between two poses is read
as the equivalent turn nearest the body's last spin, so a negated quaternion (the same pose) or a
yaw angle wrapped at 360 degrees does not read as a fast spin the other way, and a body that spins up
past half a turn per step keeps its direction. After the step, the plugin applies
the pressure impulse integrated over the body's surface to dynamic bodies as an `ExternalImpulse`,
before the physics step. That impulse covers buoyancy, drag and the push back on a paddle. The force
is the adjoint of the solid term in the divergence, `(1 - open) (p_lo - p_hi) dx²` per face. A
floating box settles at the Archimedes depth, and a box heavier than the liquid sinks. Set the
simulation's `dt` to the fixed step. `coupled_bodies` shows each body's `motion`, `supported` (false
for other collider shapes, which do not act on the liquid), `two_way`, `simulated`, `applied` and
the impulses. A dynamic body gets impulses from its second fixed step on (`simulated`), because the
physics step sets up a new body's mass in its first step. A simulation that does not step (not
running, or playing a cache) gives its bodies no impulse.

```text
fluid.create     { "bounds": { "min": [0, 0, 0], "max": [0.8, 0.4, 0.1] }, "cell_size": 0.025 }   -> { "id": 1 }
fluid.add_emitter { "id": 1, "center": [0.4, 0.1, 0.05], "half_extents": [0.4, 0.1, 0.05] }
fluid.set_solids { "id": 1, "solids": [{ "shape": "box", "center": [0.4, 0.13, 0.05],
                   "half_extents": [0.03, 0.08, 0.2], "velocity": [0.5, 0, 0] }] }
fluid.step       { "id": 1, "steps": 20 }   -> { "moving_solids": 1, "wetted_faces": ..., ... }
fluid.solids     { "id": 1 }                -> { "solids": [{ "impulse": [-0.05, 0.3, 0], ... }] }
```

The paddle pushes the liquid along +x (`fluid.step`'s mean velocity follows it). The x impulse on the
paddle is negative, because the liquid resists it. The digest covers the solids and their impulses,
so equal inputs give equal digests.

## GPU path

With the `gpu` feature of `regia_fluid`, `regia_fluid::gpu::GpuFluid` runs the grid transfers, a
red-black Gauss-Seidel pressure solve and the advection in wgpu compute shaders. It is checked against
the CPU reference within a documented tolerance (total volume and mean velocity, ADR-2441 § 4), not
bit for bit. The CPU reference is the contract. Android uses the CPU path with its lower caps:
`GpuFluid::new` refuses there with `fluid_gpu_mobile` (library API; the other GPU codes are
`fluid_gpu_too_big`, `fluid_gpu_readback` and `fluid_gpu_too_many_solids`).

The GPU path takes up to 16 moving solids (`GpuFluid::set_moving_solids`). A `rasterize` kernel poses
them at each substep and narrows each face's static open fraction by their coverage. Faces they close
carry their velocity where they are at least as near as the static colliders (a floor's closed faces
stay at rest), and particles are pushed out of them. As on the CPU, a cell whose centre is inside a
solid and that touches the liquid counts as liquid. The particle-to-grid sums are 64-bit fixed point,
so no clump of particles on one face wraps them. The pressure impulses on the solids (two-way coupling)
are computed on the CPU only.

## Format

`.fluidcache` v1: see `docs/formats/fluidcache.md`. A `.fluidcache` under `assets/` cooks into the
paks (`regia_asset_pipeline::fluid_cook`, type `regia_fluid::cache::FluidCache`). The cook validates
it with every reader check and stores it in canonical form at the current version. A damaged file is
a cook error that starts with its `fluidcache_*` code.

## Cell sleep

`fluid.sleep_mask` decides which cells of a dense speed grid to integrate this step (ADR-3949). It
does not read a simulation. The caller owns the speeds and passes the previous quiet counts back.

| Method | Params | Answers |
|---|---|---|
| `fluid.sleep_mask` | `nx`, `ny`, `nz`, `speeds`, `still?`, `epsilon?` (default 0.001), `k?` (default 4) | `{ nx, ny, nz, epsilon, k, cells, asleep, integrate, still, digest }` |

Cells are stored x-fastest, the same order as the liquid grid: flat index `i + nx * (j + ny * k)`.
`speeds` is one finite number per cell and its length is `nx * ny * nz`. The grid has at most 64³
cells. The agent contract accepts dimensions from 1 to 4096, `k` from 0 to 1,000,000, and
`epsilon` from 0 to 1,000,000. `still` is the `still` array from the previous answer; omit it
(or pass `[]`) on the first step.

A finite speed strictly under `epsilon` adds one quiet step. A speed at or above `epsilon` resets
that cell's count to zero. The library treats a non-finite speed the same way (the cell stays
awake); this method refuses a non-finite speed or `epsilon` as `fluid_bad_param`. After `k` quiet
steps the cell is asleep and is left out of `integrate`. A face neighbor that is awake because of
its own speed wakes the cell for this step, and no cell past that neighbor. The neighbor wake
does not reset the quiet count. `k` of 0 sleeps nobody: `integrate` lists every cell. `asleep` is
the number of cells left out. `digest` is 16 hex digits. Two calls with equal inputs answer the
same digest.

```text
fluid.sleep_mask { "nx": 4, "ny": 4, "nz": 4, "k": 3, "epsilon": 0.001, "speeds": [0, 0] }
```

That call is `fluid_sleep_bad_grid` (64 cells, 2 speeds). Three calls with 64 zeros and `k` 3 answer
`integrate: []` and `asleep: 64`. The next call with one speed at or above `epsilon` lists that cell
and its six face neighbors.

The solver does not call this yet. After the fluid lanes merge, the lead inserts this line after
`mark_liquid` in `FluidSim::substep`:

```text
let sleep = crate::sleep::cells_to_integrate(self.dims.nx, self.dims.ny, self.dims.nz, &cell_speed, &cell_still, epsilon, k);
```

`cell_speed` is one speed per cell. `cell_still` is empty on the first step and `sleep.still` after
that. Defaults are `regia_fluid::sleep::DEFAULT_EPSILON` (0.001) and `DEFAULT_QUIET_STEPS` (4).

| Reason | When |
|---|---|
| `fluid_sleep_bad_grid` | `speeds` or a nonempty `still` has a length other than `nx * ny * nz`, or that product does not fit. |
| `fluid_bad_param` | Required inputs are missing, a speed or `epsilon` is not a finite `f32`, or a supplied count does not fit its integer type. |
| `fluid_too_many_cells` | `nx * ny * nz` is over 64³. |
