# Lockstep projectiles and aim

`regia_detsim::projectile` is a projectile system for a lockstep simulation (ADR-8509): shells,
grenades, bullets and thrown objects that every peer steps to the same bits. `regia_detsim::aim`
solves launch angles for a target, with and without drag. Read "Lockstep math and the determinism
scan" first: the same rules apply to code that calls them.

Cosmetic projectiles (shell casings, tracers, debris that never changes the game) do not need this:
they belong to the renderer and the rigid-body physics.

## One projectile

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the module doctest of regia_detsim::projectile runs the same calls -->
```rust,ignore
use regia_detsim::projectile::{self, HitPlane, LinearScanHits, ProjectileParams, ProjectileState};
use regia_detsim::site_physics::WorldPhysics;
use regia_detsim::tick::tick_dt;

let grenade = ProjectileParams {
    radius_m: 0.05,
    restitution: 0.45, // keeps 45 % of the normal speed on a bounce
    friction: 0.3,     // takes 30 % of the tangential speed
    max_bounces: 4,
    fuse_s: Some(2.5),
    ..ProjectileParams::DEFAULT
};
grenade.validate()?; // once, when the kind is loaded
let mut hits = LinearScanHits::default();
hits.planes.push(HitPlane::ground(0.0));
let mut state = ProjectileState::launch([0.0, 1.5, 0.0], [12.0, 8.0, 0.0], Some(shooter_id));
// each tick:
if let Some(impact) = projectile::step(&grenade, &WorldPhysics::EARTH, wind, &mut state, tick_dt(), &mut hits) {
    explode_at(impact.point);
}
```

`step` does, in this order: gravity times `gravity_scale`; quadratic drag against the air, which moves
with `wind * wind_response`; linear drag; a swept query from the old position to the new one; a
bounce, a rest or an end at a contact; the age; then the fuse, the lifetime and the range.

| Field | Meaning | Valid |
|---|---|---|
| `gravity_scale` | Multiplies the world's gravity (0: no drop) | 0 to 100 |
| `drag_per_m` | Quadratic drag at 1.2 kg/m³, scaled by the world's air density, zero in vacuum | 0 to 10 |
| `linear_drag_per_s` | Linear drag at 1.2 kg/m³, scaled the same way | 0 to 1000 |
| `wind_response` | 0: the wind is ignored; 1: the drag pulls the shot toward the wind's velocity | 0 to 1 |
| `radius_m` | Radius of the swept sphere (0: a point) | 0 to 100 |
| `lifetime_s` | Ends with `expired` on the tick it is reached (0: none) | 0 to 86400 |
| `max_range_m` | Ends with `range` after this much path, bounces included (0: none) | 0 to 1e7 |
| `fuse_s` | Ends with `fuse` on the tick it is reached, in flight or at rest (`None`: no fuse) | above 0, at most 86400 |
| `restitution` | Share of the normal speed a bounce keeps | 0 to 1 |
| `friction` | Share of the tangential speed a bounce takes | 0 to 1 |
| `max_bounces` | Bounces before it stops at a contact | 0 to 65535 |
| `stop_speed_m_s` | A bounce that leaves it this slow stops it | 0 to 1e5 |

`ProjectileParams::DEFAULT` is the old debris step: quadratic drag 0.002 per m, the world's gravity,
no wind, a point, no timers, no bounces. With it the velocity is `ballistic::step_debris`'s, bit for
bit, so a game that moves from the old call to this one keeps its replays.

**Stopping.** With no bounce left (or after a bounce at or under `stop_speed_m_s`), a fused projectile
rests where it hit and waits for its fuse; an unfused one ends with a `hit` impact. There is no rolling.
A bounce drops the rest of that tick's travel (the projectile waits at the contact for one tick).

**Timers count ticks.** A timer of `t` seconds fires at the end of tick `ceil(t / dt)` after the launch
(`projectile::timer_ticks`; a quotient within 1/1000 of a tick above a whole number rounds down, so
0.1 s at 60 Hz is 6 ticks on every peer). The state keeps `age_ticks`, not seconds.

**Range is path length.** `max_range_m` counts the distance flown along the path, bounces included, not
the distance from the launch point. A shot that ends on range in flight is put back to exactly that
much path.

**Order of endings in one tick.** A hit wins over a timer; then the fuse, then the lifetime, then the
range.

## Hits: the `SegmentHits` seam

The step never looks at the world itself: it asks a `SegmentHits` for the first contact of a sphere of
`radius_m` moving from the old position to the new one (a `SegmentQuery`). Because the whole segment is
tested, a 2000 m/s shot at 60 Hz (33 m per tick) still stops at a 5 cm wall.

| Type | What it is |
|---|---|
| `SegmentHits::first_hit(&query) -> Option<Hit>` | The trait. `Hit { t, point, normal, target }`: `t` is the fraction of the segment, `point` the sphere's centre at the contact, `normal` points out of what was hit, `target` is `HitTarget::World` or `HitTarget::Body(id)`. |
| `LinearScanHits { planes, boxes, spheres }` | The engine's reference implementation: half-spaces, axis-aligned boxes (grown by the radius; at edges and corners the grown box is square, so a grazing shot may touch a little early) and body spheres, each tested in turn. |
| `NoHits` | Nothing to hit. |
| `(world, bodies)` | A pair of sources is a source: the earlier contact; at the same `t`, the first. |

Rules for an implementation (a game's spatial index for its bodies, its level's collision): the same
query on the same contents answers the same on every peer; ties at the same `t` are broken by a fixed
rule; a contact is reported only for a sphere moving into the surface; `SegmentQuery::ignore_body` (the
shooter, from `ProjectileState::owner`) is skipped. Check your index against `LinearScanHits` on the same
shapes.

## Many projectiles: `ProjectilePool`

| Call | What it does |
|---|---|
| `ProjectilePool::new(capacity)` | Slots `1..=capacity` (your setting). Everything is sized here: no heap allocation afterwards. |
| `spawn(params, state)` | The oldest freed slot, else the lowest unused one (a `DetIdAllocator`); `None` when full. |
| `spawn_in(slot, params, state)` | A fixed slot (a replay, a host-assigned slot); `false` when it is live. |
| `step_all(world, wind, dt, hits, &mut impacts)` | Steps the live slots in ascending order and appends each impact with its `slot` and `order`; frees the ended slots. |
| `despawn(slot)`, `get(slot)`, `get_mut(slot)`, `iter()`, `len()`, `capacity()` | The rest. `iter` walks the live slots in ascending order (draw them, or hash a snapshot). |

Impacts come out in ascending slot order, so two peers that put the same shots in the same slots get
the same impact list, whatever order they spawned them in. Give `step_all` a `Vec` with room for
`capacity()` impacts and clear it each tick: the tick then allocates nothing.

## Launchers and the old mortar functions

`ballistic::ArcLauncher { muzzle_m_s, earth_gravity_units, loft_share, forward_share }` is a launcher
that fires a fixed-shape arc: `range_m(mode, g)`, `gravity(g)`, `charge_scale(mode, g)`,
`range_in(world)`. `ArcLauncher::LEGACY_MORTAR` holds the numbers of the old functions (120, 32, 0.4,
0.9). `ballistic::mortar_range_m`, `mortar_gravity`, `mortar_range_in`, `EARTH_MORTAR_G` and
`MORTAR_MUZZLE_M_S` are deprecated wrappers over it, with the same bits; pass your own launchers.

## Aim

| Function | What it does |
|---|---|
| `aim::launch_angles(delta, speed, g)` | The drag-free `(low, high)` pitches (radians) that put a shot through `delta`, or `None` out of reach. The standard closed form, `tan θ = (v² ∓ √(v⁴ − g(g x² + 2 y v²))) / (g x)`. |
| `aim::time_of_flight(delta, speed, pitch, g)` | Drag-free time to the target. |
| `aim::yaw_to(delta)`, `aim::launch_velocity(delta, speed, pitch)` | The heading and the launch velocity. |
| `aim::solve_with_drag(params, world, delta, speed, iterations, &DragAim)` | The pitch that hits with the projectile's drag: starts at the drag-free pitch of `DragAim::arc`, then at most `iterations` secant steps (bisection inside a known bracket), each probe a fixed-tick flight of `projectile::step`. `AimSolution { pitch, yaw, velocity, time_s, miss_m, probes }`, or `None` when the best probe misses by more than `tolerance_m`. |

The closed form is the continuous path: a fixed-tick shot lands within about `g · t · dt / 2` of it
(0.25 m for a 3 s flight at 60 Hz). `solve_with_drag` flies the real tick step, so it closes that gap
too (pass `drag_per_m: 0` for a drag-free solve on the tick).

All of them are pure functions: the simulation (an AI choosing its pitch on a tick) and the UI (a
trajectory arc, aim assist) may both call them and get the same bits. The arc a UI draws is
presentation; the shot is what the simulation steps. `solve_with_drag` flies up to
`(iterations + 2) * max_ticks` ticks: call it when an AI picks a target, not per projectile per tick.

## Agent methods

`detsim.projectile_simulate` flies one projectile and answers its path; `detsim.aim_solve` answers the
angles. Both are stateless and read-only. Both take `rate_hz` (the tick rate, 1 to 1000, default 60);
`params` and `world` take the fields of `ProjectileParams` and `WorldPhysics` (missing fields: the
default projectile, Earth), and `world.low_g` is `"governor"` or `"physical"`.

```json
{"method": "detsim.projectile_simulate", "params": {
  "launch": {"pos": [0, 1, 0], "vel": [20, 15, 0]},
  "params": {"radius_m": 0.05, "restitution": 0.5, "max_bounces": 2},
  "world": {"gravity_m_s2": 9.81, "air_density_kg_m3": 1.2},
  "geometry": {"ground_y": 0, "boxes": [{"min": [30, 0, -5], "max": [30.05, 4, 5]}]},
  "steps": 600, "sample_every": 10}}
```

The answer: `ticks`, `dt`, `alive`, `travelled_m`, `samples` (`tick`, `pos`, `vel`, `bounces`,
`resting`; the last tick is always sampled) and `impact` (`tick`, `reason`: `hit`, `fuse`, `expired`
or `range`; `point`, `normal`, `target`: `"world"`, `{"body": id}` or null; `speed`, `age_ticks`) or
null. `geometry` takes `ground_y`, `planes` (`normal`, `offset`), `boxes` (`min`, `max`) and `spheres`
(`center`, `radius_m`, `body`), at most 256 of each.

```json
{"method": "detsim.aim_solve", "params": {
  "delta": [150, 5, 40], "speed": 80, "params": {"drag_per_m": 0.004},
  "drag": true, "arc": "low", "iterations": 8}}
```

The answer: `reachable`, `low`, `high`, `time_low`, `time_high` (null when out of reach), `yaw`,
`gravity` (the world's times `gravity_scale`) and `solution` (`pitch`, `yaw`, `velocity`, `time_s`,
`miss_m`, `probes`) or null.

| Error code | When |
|---|---|
| `bad_params` | A field is missing or has the wrong shape, a projectile number is invalid (`data.kind` is `not_finite`, `negative` or `out_of_range`, as `ProjectileParamsErrorKind::code`), or the world is invalid. `data.field` names the field. |
| `steps_limit` | `steps` is 0 or over 36000, the samples would be more than 4096 (raise `sample_every`), `iterations` is over 32, or `max_ticks` is 0 or over 36000. |

## Check it

```sh
cargo test -p regia_detsim --test projectile
cargo test -p regia_detsim --lib projectile
cargo test -p regia_test_utils --test det_projectile_alloc
cargo test -p regia_agent --lib det_projectiles
```

`cargo run --release -p regia_detsim --example projectile_cost -- <capacity> <ticks> [bodies]` prints the
cost of a full pool's tick against the reference hits (evidence, not a budget).

The `projectile_mix` golden scenario (seeded shots of four kinds against ground, a slope, a thin wall,
a crate and four bodies, under changing wind, on four worlds) pins the bits per target: see
"Determinism goldens". `cargo xtask det-golden run --legs host,qemu` runs it with the other engine
scenarios.
