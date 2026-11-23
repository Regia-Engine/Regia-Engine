# Hover step (lockstep)

`regia_detsim::hover` is a hover hull's movement for a lockstep simulation: one pure function per
fixed tick, the same bits on every OS and CPU architecture (ADR-8012 A-4, ADR-8079). It is the hover
step of Direct Order 27, moved into Regia with neutral parameter names; its arithmetic is unchanged,
so a game that passes the same parameters gets the same bits. The engine's own default hull and every
default number are Regia's (ADR-8164); a game passes its own hull at runtime.

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the module doctest of regia_detsim::hover compiles and runs a step loop like it -->
```rust,ignore
use regia_detsim::hover::{self, FlightMode, HoverParams, HoverState};
use regia_detsim::tick::tick_dt;

let params = HoverParams::DEFAULT_HULL; // or a game's own hull
let climb = FlightMode::AirTime.climb(); // a match setting: every peer uses the same one
let mut state = HoverState::default();
let mut pos = [0.0_f32; 3];
// Each tick: input = [throttle, strafe, steer] in -1..1, floor = ground height under the hull.
let (input, floor) = ([1.0, 0.0, 0.2], 0.0);
hover::step_tuned(&params, &mut state, &input, floor, tick_dt(), 9.81, 1.2, climb);
pos[0] += state.velocity[0] * tick_dt();
pos[2] += state.velocity[2] * tick_dt();
pos[1] = floor + state.altitude;
```

The step owns velocity, yaw, roll, cushion pitch and the altitude above ground; the caller owns the
position (the lines after the call). `hover::step` is the same with Earth-like gravity 9.8, no air and
the default flight rules.

| Function | What it does |
|---|---|
| `step(params, state, input, floor, dt)` | One tick, gravity 9.8, vacuum, air-time flight. |
| `step_world(.., gravity, air)` | One tick on a world with that gravity (m/s²) and air density (kg/m³). |
| `step_tuned(.., gravity, air, climb)` | One tick with the flight rules given (`FlightMode::climb()`). |
| `apply_jump(state, params)` | A jump impulse of `params.jump_speed` (no-op in the air). |
| `hop_launch_speed(gravity, air, dt)`, `apply_hull_hop(state, speed)` | A 3 m fan hop on every world, one per press, 0.8 s cooldown; a press while falling softens the landing. |
| `compute_lean(params, state, input)` | The visual pitch and roll (presentation only). |
| `hull_in_air`, `cushion_reach` | Whether the hull is off its cushion; how much of the ground tilt the cushion still follows. |

`FlightMode` is `"air_time"` (the default: a hull keeps its upward speed over a crest and falls at
about 6 m/s² until the cushion catches it) or `"clamped"` (hulls hug the ground, hops of 2 m at most).
`FlightMode::from_name` refuses any other string, so a typo in a settings file never picks a mode.

The modules beside it: `ballistic` (one ballistic step for jumps, falls, debris and mortar arcs, with
quadratic air drag), `site_physics` (a world's gravity as fan-load and jump-impulse scales) and
`lowg` (mode A keeps the Earth jump height and mortar range on every world, mode B flies true arcs;
hover thrust from air density, zero in vacuum).

## Which hover?

Regia has two hovers. This page's step (`regia_detsim::hover`) is the lockstep one: an arcade model with
the same bits on every CPU architecture, the one a lockstep simulation must use. `regia_physics::hover` is a
physical craft held up by ray-cast thrusters on a rapier chassis ([Hover craft](vehicles.md#hover-craft),
`physics.hovercraft`): it pushes and is pushed by other bodies, but it gives the same bits only on the same build
and CPU architecture, so it belongs to server-authoritative and single-player games. A lockstep simulation never
reads a rapier pose (ADR-8841). The table per genre is in
[Movement models](../user-guide/movement-models.md).

## Parameters

`HoverParams::DEFAULT_HULL` (also `HoverParams::default()`) is an engine-authored generic 30 t hover
vehicle in round SI numbers, fitted to no game (ADR-8164; the derivation is in its doc comment): 30 m/s
forward, 10 reverse, 15 strafe, 1 m ride height on a 5 rad/s cushion spring, 15 m/s² of thrust tapering
near the top, 25 m/s² of braking, a quick release drop then a 4 m/s² coast, 3 rad/s spin at rest and 1.5
at speed. Start a game's hulls from it (`HoverParams { max_forward_speed: 25.0, ..HoverParams::DEFAULT_HULL }`)
or pass a full set of your own: every number of the hull, including its cushion and roll springs
(`cushion_*`, `roll_spring_*`) and its release drop (`coast_drop*`), is a field. The climb and crest
numbers are per match in `ClimbTuning` (`ClimbTuning::AIR_TIME` is the default). The model's own limits
(hop, air steer, thresholds, crest, grade and attitude limits) are a third value, `HoverLimits`
([Model limits and worlds](#model-limits-and-worlds)); `hover::tuning` keeps only the internal
numbers no game needs to change (changing one is a sim-version change, ADR-8019).

With the crate's `serde` feature, `HoverParams` and `HoverState` serialize; the field order is fixed,
so a positional encoding keeps its bytes across versions that add no field.

## Check it

```sh
cargo test -p regia_detsim --test hover_libm_1000 -- --nocapture
```

This is the cross-OS proof. `libm_1000_tick_vector_matches_committed_hash` steps the default hull
at full throttle from rest for 1000 ticks at 60 Hz and compares the final position and velocity, as
the hex of their f32 bits, with the committed line (`tests/fixtures/hover/libm-1000.txt`, recorded on
x86-64 Linux). It prints one line an agent or a runner can grep:

```text
regia-hover-libm-1000 target=x86_64-linux hash=00000000_3f7ffff7_43e839aa_00000000_35e10003_41f00000 expected=00000000_3f7ffff7_43e839aa_00000000_35e10003_41f00000
```

`hash` equal to `expected` means this target steps the hover exactly as the x86-64 Linux build does.
A game pins the same vector on its own hull: `HoverLibm1000::with_params(its_hull)` runs it as a
golden scenario in the game's own repository. The
fixture is compiled into the test binary, so a binary cross-built for aarch64 runs alone under
qemu-user, on the Android device (`adb push` and run) or on a Mac.

The same run is the golden scenario `hover_libm_1000` ([Determinism goldens](det-golden.md)): it
hashes the position and every `HoverState` word after each tick, so a drift is reported with its
first diverging tick and the fields that moved. It prints a `regia-det-golden {json}` line with the
status codes of that page (`ok`, `unrecorded_target`, `diverged`, `golden_missing`, ...).
`rate_60_vs_120_on_the_hover_step` runs the 60 Hz vs 120 Hz gates on the hover step.

The step keeps no runtime state of its own; the agent methods `detsim.hover_step` and
`detsim.hover_probe` run it statelessly, and `detsim.hover_list`, `detsim.hover_get` and
`detsim.hover_set_input` read and drive the hulls of an ECS app (see "Hover hulls in an ECS app" below).

## Rules for code that calls it

The hover step follows the lockstep rules ([Lockstep math and the determinism scan](lockstep-math.md)):
trigonometry through `regia_detsim::math`, no fused multiply-add, no SIMD types. Code that integrates
the position or feeds the floor height must follow them too, or the bits it hands the step differ
between peers. Keep the call order fixed: step, then position, every tick, for every hull in a fixed
order.

## Bad input

A garbled input frame, a host bug or a mod's hull file must not leave a hull stuck with a NaN state for
the rest of a match. The step guards its inputs before any arithmetic; for a finite state, finite
inputs and floor and a finite `dt > 0` the guards change nothing, so the libm-1000 line above and the
goldens keep their bits.

| Bad value | What the step does |
|---|---|
| `dt` NaN, infinite, 0 or negative | Nothing: the state is left unchanged (as `ballistic::step_debris` does). |
| An input axis NaN or infinite | Reads it as `0.0`. A finite axis outside -1..1 is clamped, as before. |
| `floor` NaN or infinite | Reads the previous floor (`state.prev_terrain`, or 0 when it is unset). |
| `gravity` or `air` NaN or infinite | Reads `hover::GRAVITY` (9.8) or 0 (vacuum). |
| A NaN or infinite word already in the state (a host write, or a tick stepped with an invalid hull) | Resets the hull to rest at the floor: `HoverState::default()` with `altitude` = the ride height and `prev_terrain` = the floor, keeping the host's `level_off` and `yaw_modern` flags. That tick is not stepped. |

`hover::try_step_tuned` (same arguments as `step_tuned`) returns a `StepReport { recovered }` that is
`true` on the reset tick: log it, every peer resets on the same tick. `step`, `step_world` and
`step_tuned` run the same guards and drop the report.

The step does not validate the hull (it runs every tick). Validate a hull when you load it, and step
the sanitized numbers:

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; validate_names_the_bad_field in regia_detsim runs these calls -->
```rust,ignore
let hull: HoverParams = ron::from_str(&text)?; // a mod's hull file (crate feature `serde`)
if let Err(e) = hull.validate() {
    eprintln!("{e}"); // e.g. "out_of_range: hover field `height_spring` = 5000 (valid 0 to 400)"
}
let hull = hull.sanitized(); // what every peer steps
```

`HoverParams::validate()` returns the first bad field in declaration order as a `HoverParamsError`
(`field`, `kind`, `value`, `range`); `ClimbTuning::validate()` and `FlightRules::validate()` do the
same for the per-match numbers (`ClimbTuning` reports its flight fields as `flight.ride_max_m`, ...).
Each field's valid range is in its doc comment and in the `FIELD_RANGES` table the checks read.

| Code | Meaning |
|---|---|
| `not_finite` | The field is NaN or infinite. |
| `negative` | Below 0 where the range starts at 0. |
| `zero_rate` | 0 for a rate or time constant that must be above 0 (`coast_drop_tau`, `roll_spring_omega`, `cushion_reach_band_m`). |
| `out_of_range` | Outside the field's documented range (above its maximum, or below a negative minimum). |
| `inverted_range` | `yaw_deadband` above `yaw_rate_at_rest`: full lock at full speed would not turn at all. |

`sanitized()` gives a NaN or infinite field the default value (`HoverParams::DEFAULT_HULL`,
`ClimbTuning::AIR_TIME`, `FlightRules::AIR_TIME`), clamps an out-of-range value to the nearer bound (a
zero or negative rate to 0.001), and clamps `yaw_deadband` to `yaw_rate_at_rest`. A valid hull comes
back bit for bit, so sanitizing never moves a valid hull's run. The spring and damper maximums keep
the cushion and roll springs stable at 30 Hz and faster: with sanitized numbers, floors within ±1e9 m
and a `dt` of at most 1/30 s, every state word stays finite, whatever the inputs.

`ballistic::landing_speed` (up to 200,000 steps on a low-g world) and `hover::hop_launch_speed` (24
bisection steps, each flying a whole ascent) are bounded tools: call them once per world and cache
the result, never per tick.

Checks: `cargo test -p regia_detsim --lib hover::tests` (`a_zero_or_nan_dt_leaves_the_state_unchanged`,
`a_nan_input_or_floor_never_reaches_the_state`, `a_poisoned_state_recovers_to_rest_at_the_floor`,
`validate_names_the_bad_field`, `sanitized_hull_steps_finite_for_random_params`,
`an_absurd_roll_spring_terminates_and_recovers`) and the fuzz target `hover_params_step`
([Fuzzing](fuzzing.md); a hull as RON plus up to 64 ticks: the raw hull never panics or hangs, the
sanitized hull stays finite).

## Model limits and worlds

A game sets the hover model's feel with three values, one per concern, plus the world it flies in
(ADR-8349):

| Value | What it holds | Engine default |
|---|---|---|
| `HoverParams` | The hull: speeds, cushion and roll springs, thrust, brakes. | `HoverParams::DEFAULT_HULL` |
| `HoverLimits` | The model's limits: hop apex, cooldown and soft-landing share, the hop gravity floor, the modern aim's yaw tracking, the straightening grip, air steer, the airborne and thrust thresholds, the crest, lookahead, grade and cliff limits, the attitude limits. | `HoverLimits::DEFAULT` (the values the former constants held) |
| `ClimbTuning` | The match's climb and flight rules. | `ClimbTuning::AIR_TIME` |
| `WorldPhysics` | Gravity (m/s²), air density (kg/m³), vacuum, the low-g mode. | `WorldPhysics::EARTH` (9.81, 1.2, air, mode A) |

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the doctest of regia_detsim::hover::step_with runs these calls -->
```rust,ignore
use regia_detsim::hover::{self, ClimbTuning, HoverLimits, HoverParams, HoverState, WorldPhysics};

let limits = HoverLimits { hop_apex_m: 1.5, hop_cooldown_s: 0.3, ..HoverLimits::DEFAULT };
limits.validate()?; // once, when the match settings load
let world = WorldPhysics { gravity_m_s2: 3.7, ..WorldPhysics::EARTH };
world.validate()?;
let report = hover::step_with(
    &params, &limits, &ClimbTuning::AIR_TIME, &world, &mut state, &input, floor, dt,
);
```

`hover::step_with(params, limits, climb, world, state, input, floor, dt) -> StepReport` is the full
entry point. `step`, `step_world`, `step_tuned` and `try_step_tuned` keep their signatures and their
bits: they call `step_with` under `HoverLimits::DEFAULT`. `hop_launch_speed_for(limits, world, dt)`
and `apply_hull_hop_with(limits, state, speed)` are the hop under a game's limits. Every field's unit
and valid range is in its doc comment and in `HoverLimits::FIELD_RANGES`; `HoverLimits::validate()`
and `sanitized()` use the error codes of [Bad input](#bad-input).

The former `pub const`s (`HOP_APEX_M`, `HOP_COOLDOWN_S`, `HOP_LAND_KEEP`, `JUMP_FEEL_GRAVITY_MIN`,
`AIM_MODERN_YAW_ALPHA`, and in `hover::tuning` `HOVER_CLIFF_SLOPE`, `AIR_LIFT_MAX_G`,
`FREE_ROLL_RATE_RAD_S`, `FREE_ROLL_TRACK`, `ROLL_LEAN_MAX`, `CUSHION_PITCH_MAX`, the crest, slope,
lookahead, grade, overspeed and air-attitude limits) are deprecated aliases of the `DEFAULT` fields
for one release.

**Two gravities.** `hover::GRAVITY` is 9.8: the hover step was ported with its own round number and
the libm-1000 vector, the hover goldens and the hover tests are pinned on it, so `step` (and
`hover::DEFAULT_WORLD`) keep it. `site_physics::EARTH_G` and `WorldPhysics::EARTH` are 9.81. Changing
either default is a sim-version change (ADR-8019); a host that wants 9.81 for its hulls passes
`WorldPhysics::EARTH` to `step_with`. `site_physics::TITAN_G` is deprecated (one body's number in
engine API): a game keeps its own bodies' gravity in its own data. `lowg::EARTH_JUMP_APEX_M` is the
default suit's apex only; `jump_velocity`, `arc_row_for` and the new `jump_velocity_in` and
`arc_row_in` take the game's own `earth_apex_m`.

**The world is a match setting.** Every peer of a lockstep match must step the same world and the
same limits. `WorldPhysics::to_le_bytes()` gives 10 bytes in a fixed order (gravity bits, air density
bits, both little-endian `f32::to_bits`, then `vacuum` 0 or 1, then `low_g` 0 for mode A or 1 for mode
B). A lobby, or an agent checking two runs, compares these bytes at match start and refuses a peer
whose bytes differ: compare the bytes, not `==` (IEEE `==` treats `-0.0` and `0.0` as equal).
`WorldPhysics::from_le_bytes` reads them back bit for bit and validates. The `_in` wrappers
(`ballistic::step_debris_in`, `step_per_tick_in`, `mortar_range_in`, `landing_speed_in`,
`lowg::play_check_in`, `jump_velocity_in`, `arc_row_in`) take `&WorldPhysics`; the loose-`f32`
functions stay.

| Code | Meaning (`WorldPhysicsErrorKind::code`) |
|---|---|
| `not_finite` | Gravity or air density is NaN or infinite. |
| `negative_zero` | A number is `-0.0`: two worlds that step the same must have the same bytes. |
| `out_of_range` | Gravity not in (0, 1000] m/s², or air density not in [0, 1000] kg/m³. |
| `bad_flag` | A `vacuum` or `low_g` byte other than 0 or 1 (from `from_le_bytes`). |

There is no agent-protocol method (pure values). An agent compares two runs' worlds by their
`to_le_bytes()` hex.

Checks: `cargo test -p regia_detsim --lib hover::tests_limits` (`limits_default_equals_the_old_constants`,
`step_with_default_limits_is_bit_identical_to_step_tuned` over a 3600-tick mixed stream,
`a_game_can_change_the_hop_apex_and_cooldown`, `every_limit_reaches_the_step`,
`limits_validate_and_sanitize`, `world_physics_bytes_round_trip_raw_bits`,
`two_worlds_that_differ_in_one_bit_have_different_bytes`,
`vacuum_ignores_the_air_density_and_world_wrappers_match_the_loose_functions`).

The feel-target measurement rig that flies this step is [Hover feel-target rig](det-hover-rig.md) (ADR-8393).

## Mixed golden

`hover_libm_1000` drives one hull straight ahead on flat ground, so it pins only the thrust path. The
golden scenario `hover_mixed` (`regia_detsim::hover::HoverMixed`, [Determinism goldens](det-golden.md))
pins the rest of the step on every target the runners reach: 12 hulls, each of
`HoverMixed::HULLS` (the default hull, a light skiff without yaw deadband and a heavy barge, both
`..HoverParams::DEFAULT_HULL`) on each of `HoverMixed::WORLDS` (gravity and air (9.81, 1.2), (3.71,
0.02), (1.35, 5.3) and (0.113, vacuum)), for 3600 ticks at 60 Hz.

| What it drives | How |
|---|---|
| Input | A seeded stream per hull, held 15 ticks: `-0.0`, `+0.0`, `+-1`, `+-1.5` (clamped), 0.05 and the value just under it (the steer and grip thresholds), the hull's deadband edge and half of it, uniform values; a mouse pitch with each draw. |
| Floor | `hover::mixed_floor(x, z)` on a 200 m course that wraps: a ramp to a 4 m crest, a 3 m ledge and drop, a 51-degree face, a 6.17 m and a 5 m step (both past `HoverLimits::grade_teleport_m`), 1.5 m ridges across. The host's ground pass writes `ground_pitch` and `ground_roll` from samples a metre around the hull. |
| Settings | `FlightMode` (air time, clamped), `level_off` and `yaw_modern` step through all eight combinations, every 450 ticks. |
| Hops and jumps | `apply_hull_hop_with` on a 240-tick schedule (one press fires, one is refused by the cooldown, one meets a hull in the air), `apply_jump` every 150 ticks. |

The golden hashes, per hull, the pose, every `HoverState` word, the input and the floor, then the
branch counts (`HoverMixed::coverage()`, a `HoverMixedCoverage`): hull-ticks airborne and on the cushion, in each flight
mode, in free air with auto-level off and with modern yaw; hops fired and refused by the cooldown;
jumps, and jump ticks under the low-g jump gravity floor; crest falls; deadband hits; teleport-sized
floor steps. `hover_mixed_reaches_every_branch` fails if any count is 0, so an edit of the scenario
cannot quietly drop a branch.

`lowg_ballistic_mixed` (`regia_detsim::hover::LowgBallisticMixed`) does the same for the low-g and
ballistic helpers on the same worlds in both `LowG` modes: `arc_row_for`, `jump_velocity`,
`step_debris` (with drag, including the drag cap), `step_per_tick` (mortar), `hop_launch_speed`.

```sh
cargo test -p regia_detsim --test det_golden -- --nocapture hover_mixed lowg_ballistic_mixed
```

Each prints its `regia-det-golden {json}` line from `engine_scenarios_match_their_committed_goldens`
(status `ok` on a target with its own line, `unrecorded_target` on one that matches the reference but
has no line yet, `diverged` with the first tick otherwise). `HoverMixed::with_input_flip(tick, bit)` is
the planted control: one flipped input bit at tick 1800 is reported at tick 1800, naming
`hull[0].in_throttle`. A target that differs is a finding, never a bless: report the first diverging
tick and field.
## Host kit

The step leaves three jobs to its caller each tick (ADR-8508): read the ground's tilt under the hull,
read the floor height, and move the hull. `regia_detsim::hover::host` does them, so a host does not
write its own:

| Item | What it does |
|---|---|
| `FloorSampler` (`fn floor(&self, x, z) -> f32`) | The floor height at a point. Must be lockstep code (the same bits on every peer). |
| `FloorFn(|x, z| ...)`, `FlatFloor(h)`, `HeightGrid` | A closure, a flat floor, a regular heightfield, as samplers. |
| `integrate(body, dt, floor)` | `x += vx * dt`, `z += vz * dt`, then `y = floor + altitude`: the libm-1000 loop, bit for bit. `floor` is the height the step was given this tick. |
| `ground_attitude(sampler, pos, yaw, half_length, half_width) -> (pitch, roll)` | The ground pass: writes nothing, returns what goes into `HoverState::ground_pitch` and `ground_roll`. |
| `HoverFleet` | Every hull of a match in slot order: `push(body, hull_type) -> slot`, `step_all(inputs, sampler, world, limits, climb, dt) -> FleetReport`. |

<!-- ignore-reason: regia_detsim is not a dependency of the book snippet crate; the module doctest of regia_detsim::hover::host runs a fleet like it -->
```rust,ignore
use regia_detsim::hover::host::{FleetHull, FleetInput, HeightGrid, HoverBody, HoverFleet};
use regia_detsim::hover::{ClimbTuning, HoverLimits, HoverParams, WorldPhysics};

let floor = HeightGrid::new(origin, cell, cols, rows, heights)?; // or FloorFn(|x, z| ...)
let mut fleet = HoverFleet::new(vec![FleetHull::new(HoverParams::DEFAULT_HULL, 2.0, 1.0)]);
let slot = fleet.push(HoverBody::at_rest([x, floor_y, z], yaw, &HoverParams::DEFAULT_HULL), 0);
// Each tick, inputs in any order (arrival order is fine):
let inputs = [FleetInput::new(slot, [throttle, strafe, steer])];
let report = fleet.step_all(&inputs, &floor, &WorldPhysics::EARTH, &HoverLimits::DEFAULT,
    &ClimbTuning::AIR_TIME, dt);
```

**Ground attitude.** Four floor samples on the hull's yaw basis (forward `(sin yaw, cos yaw)`, right
`(cos yaw, -sin yaw)`): nose and tail `half_length` ahead and behind, right and left `half_width` to
the sides. `pitch = atan2(nose - tail, 2 half_length)`, `roll = atan2(right - left, 2 half_width)`,
through `regia_detsim::math`: a central difference, exact for a plane. Pitch is positive when the
ground rises ahead, roll when it rises on the right. A NaN or infinite sample reads the center floor
(sampled only then), and a bad center reads 0; a footprint that is not finite and above 0 gives 0 for
that axis; results are never `-0`. Without this pass `ground_pitch` and `ground_roll` stay 0 and a
hull never tilts with the ground. On the cushion the hull's roll settles at
`-cushion_terrain_share * cushion_reach * ground_roll` (ground rising on the right lifts the right
side, a negative roll) and its cushion pitch at `cushion_terrain_share * cushion_reach * ground_pitch`.
This is Regia's own pass; a game with its own pass keeps it and writes the two words itself.

**HeightGrid index convention.** `heights` is row-major: column `c` runs along +X, row `r` along +Z,
post `(c, r)` is `heights[r * cols + c]` at world `(origin[0] + c * cell, origin[1] + r * cell)`. A
point is clamped to the grid (off the grid reads the nearest edge; a NaN coordinate reads as 0), and
the four posts of its cell are blended in one fixed order: along X on the near row, along X on the far
row, then along Z. A point on a post reads that post's height bit for bit. `HeightGrid::new` refuses a
grid under 2 x 2 (`TooSmall`), a cell that is not finite and above 0 or a non-finite origin
(`BadCell`), and a height count that is not `cols * rows` (`WrongLength`).

**Fleet order.** `step_all` steps the hulls in ascending slot order, and for each one: ground
attitude, floor sample at the hull, `step_with` with its hull type's parameters, `integrate`. Inputs
are `(slot, axes)` entries in any order; the fleet gathers them by slot first, so their order never
moves a bit. A slot with no entry reads neutral input; a slot named twice reads neutral input too
(`duplicate_slots`), whatever the order; an entry for a missing slot is ignored (`unknown_slots`). A
`dt` that is not finite and above 0 steps nothing (`skipped`). `recovered` counts hulls reset by the
step's bad-state guard. The fleet's buffers grow only in `push`: `step_all` makes no heap allocation.
Hulls do not collide here (contacts are a later pass).

**Golden.** `hover_fleet_terrain` (engine scenario six): 64 hulls of two types on an 8 x 8 grid over
`RollingField` (rolling hills on a tilt, through `regia_detsim::math`), seeded inputs redrawn every 20
ticks and handed over in descending slot order, every sixteenth hull without input, a jump every 120
ticks, air-time rules then clamped rules, 1800 ticks at 60 Hz on `WorldPhysics::EARTH`. It hashes every
hull's position and every state word.

The fleet itself has no agent-protocol method (pure functions); the `regia-det-golden {json}` line of
`hover_fleet_terrain` is its agent surface, and the `detsim.hover_*` methods below step the same tick.

Checks: `cargo test -p regia_detsim --test hover_host` (`ground_attitude_on_a_plane`,
`ground_attitude_is_nan_safe`, `a_hull_on_a_side_slope_rolls_with_it`, `fleet_order_is_slot_order`,
`height_grid_index_convention_and_bilinear`, `height_grid_rejects_bad_shapes`, `a_fleet_over_holes_in_the_floor_stays_finite`,
`hover_fleet_terrain_matches_its_committed_golden`), `cargo test -p regia_detsim --test
hover_libm_1000 integrate_matches_the_libm_1000_loop_bit_for_bit`, and `cargo test -p
regia_test_utils --test hover_fleet_alloc` (`fleet_steps_allocate_nothing_after_warmup`, the shared
counting allocator).

**Cost.** `cargo run --release -p regia_detsim --example hover_fleet_cost -- 600 1 100 900 2000`
prints the mean ns per hull and the mean, p50, p99 and max tick for each size, on `RollingField` and on
a 512 x 512 `HeightGrid`. The budget line to compare against: 900 hulls inside 1.0 ms of a 4.0 ms p99
tick.

Measured 2026-11-18, release builds, 600 timed ticks after 60 warm-up ticks (mean ns per hull; p99 tick in ms):

| Host | Floor | 1 hull | 100 hulls | 900 hulls | 2000 hulls |
|---|---|---|---|---|---|
| Laptop, Apple M5 (aarch64 macOS) | `RollingField` | 453 ns | 289 ns (0.037 ms) | 150 ns (0.179 ms) | 142 ns (0.336 ms) |
| Laptop, Apple M5 (aarch64 macOS) | `HeightGrid` 512² | 386 ns | 166 ns (0.021 ms) | 105 ns (0.099 ms) | 106 ns (0.225 ms) |
| Min-spec laptop, Intel N150 (x86-64 Windows, MSVC) | `RollingField` | 427 ns | 430 ns (0.048 ms) | 472 ns (0.739 ms) | 469 ns (1.174 ms) |
| Min-spec laptop, Intel N150 (x86-64 Windows, MSVC) | `HeightGrid` 512² | 329 ns | 311 ns (0.036 ms) | 322 ns (0.522 ms) | 319 ns (1.019 ms) |
| Build server, 128 threads (x86-64 Linux; shared, load 200-237, `nice 19`) | `RollingField` | 467 ns | 513 ns (0.076 ms) | 559 ns (0.640 ms) | 579 ns (1.294 ms) |
| Build server, 128 threads (x86-64 Linux; shared, load 200-237, `nice 19`) | `HeightGrid` 512² | 422 ns | 398 ns (0.065 ms) | 404 ns (0.555 ms) | 428 ns (1.448 ms) |

On the min-spec laptop 900 hulls take 0.42 ms per tick on average (0.74 ms p99) on the analytic floor and 0.29 ms
(0.52 ms p99) on a grid: inside the 1.0 ms share of a 4.0 ms p99 tick. The shared build server, measured while it ran other builds,
gives the same picture (0.64 ms and 0.55 ms p99 at 900 hulls). The analytic floor costs more than the grid
because each of its five samples per hull (four for the attitude, one for the step) runs three `sin`/`cos` calls.
Not measured yet: the Steam Deck (unreachable) and the test Pixel 9 Pro (not on adb); the commands are in the
lane report.

## Hover hulls in an ECS app

`regia_framework::det_hover` runs the hover in an engine-native app (ADR-8550): no glue for a game to write.

| Item | What it is |
|---|---|
| `DetHoverHull { params, limits, flight, half_length, half_width }` | A hull's tuning: `HoverParams`, `HoverLimits`, `FlightMode` and the footprint the ground pass samples. `DetHoverHull::DEFAULT` is the engine's generic hull on a 4 m x 2 m footprint; `validate()` names the first bad field with the codes of "Bad input" (the footprint fields are `half_length` and `half_width`, 0 to 100 m). |
| `DetHoverId(u64)` | The hull's sim id. Hulls step in ascending id order, never archetype or spawn order. A hull without one is not stepped. |
| `DetHoverState(HoverBody)` | The sim state (position and `HoverState`): the only thing the system reads and writes. Save it to roll back. |
| `DetHoverInput { throttle, strafe, steer, hop, jump }` | Next tick's input. `hop` and `jump` are read every tick they are set (a held hop fires again when its cooldown ends). |
| `DetHoverLean { pitch, roll }` | The visual lean (`compute_lean`), written after each step, never read by the sim. |
| `DetHoverFloor` | The floor sampler every hull flies over (flat 0 by default). `DetHoverFloor::from_terrain(&map)` wraps a terrain heightfield (`TerrainFloor`, client builds); `DetHoverConfig::follow_terrain` swaps it in when a `Terrain` is added or changed. |
| `DetHoverConfig { world, dt, follow_terrain }` | `WorldPhysics::EARTH` and `tick_dt()` (1/60 s) by default. `dt` is never the frame delta. |
| `DetHoverStats` | `ticks`, `hulls`, `recovered`, `hops`, `jumps`, `duplicate_ids`, `unordered`, `skipped`. |
| `tick_hull(hull, body, input, floor, world, hop_speed, dt)` | One tick of one hull: the pure function the system, the agent and a predictor run. |

`DetHoverPlugin` adds, in `FixedUpdate`, `DetHoverSet::Step` (every hull one `tick_hull`, by id) then
`DetHoverSet::Present` (`DetHoverLean`, and the `Transform`: translation = position, rotation = yaw, then the cushion
pitch plus the lean pitch, then the roll plus the lean roll). Write `DetHoverInput` before `DetHoverSet::Step`. Render
interpolation between fixed ticks is the engine's (`PreviousGlobalTransform`).

`tick_hull` runs, in order: `ground_attitude` on the hull's footprint into `ground_pitch`/`ground_roll`, the floor
sample at the hull, `apply_jump` (when `jump` is set and the hull is not airborne), `apply_hull_hop_with` (when `hop` is
set), `step_with` with the hull's parameters, limits and flight rules, then `integrate` with the floor the step used.
That is `HoverFleet::step_all`'s order: a world of hulls that never hop or jump steps bit for bit like a fleet of the
same hulls (`det_hover_system_matches_the_fleet`). The hop launch speed is a bounded tool (`hop_launch_speed_for`), so
the system solves it once per hull limits, world and `dt` (`HopSpeedCache`).

**Determinism.** The per-tick sim states do not depend on the render frame rate (`det_hover_ignores_frame_rate`: 30, 60
and 144 fps give the same states per fixed tick). This module is **not** on the lockstep closure: it runs inside the ECS,
and the presentation half uses glam's quaternion trig. A lockstep game steps `regia_detsim` directly (`HoverFleet` or
`step_with`) inside its sim; `DetHoverPlugin` is for games that run the hover on one authority (single player,
server-authoritative) and for client prediction. The sim half (`tick_hull`) calls only `regia_detsim` and is as
repeatable as the floor: `HeightGrid`, `FlatFloor` and `FloorFn` over `regia_detsim::math` are lockstep code;
`TerrainFloor` reads `TerrainMap::sample_bilinear` (plain IEEE `+ - *` over the heightfield's `f32` mirror, repeatable
for the cooked `u16` grid or an `f32` grid, but outside det-scan's reach).

**Prediction and rollback.** `tick_hull` is a pure function of (hull, state, input, floor, world, dt). A client
predictor saves `DetHoverState` per tick, and on a correction restores the server's state and replays its buffered
inputs through the same function: the replay reaches the same bits (`det_hover_rollback_replays_bit_for_bit`, which
rolls back both the pure tick and the ECS world). There is no `net_character`-style plugin for hulls yet.

**Debug draw.** The gizmo group `det_hover` (off by default; its system does not run while off; turn it on with
`debug.set_gizmos { "group": "det_hover", "enabled": true }`) draws per hull: the ride-height line from the floor (green
on the cushion, orange in the air), a cross at the lookahead aim height (`lookahead_aim`), the four ground-pass samples
(`attitude_samples`), and two arrows from the hull along the nose (cushion pitch) and the right side (roll).

### Methods

| Method | Params | Answers |
|---|---|---|
| `detsim.hover_step` | `params?`, `limits?`, `world?`, `flight?`, `half_length?`, `half_width?`, `input`, `state?`, `floor` or `floor_profile`, `dt?`, `steps?` | `{ state, pose, report, digest }` |
| `detsim.hover_probe` | `script`, `params?`, `limits?`, `world?`, `flight?`, `half_length?`, `half_width?`, `dt?` | `{ script, dt, ticks, numbers }` |
| `detsim.hover_list` | none | `{ hulls: [{ entity, id, position, yaw, speed, airborne, in_air }], stats, plugin, dt }`, in step order |
| `detsim.hover_get` | `entity` | `{ entity, id, hull, state, pose, input, lean, digest }` |
| `detsim.hover_set_input` | `entity`, `input` | `{ entity, before, input }`; the next fixed tick reads it |

`detsim.hover_step` and `detsim.hover_probe` are stateless, like `physics.track_step`. `params` and `limits` are
`HoverParams` and `HoverLimits` fields written over the defaults (unknown fields are refused); `world` is
`{ gravity_m_s2?, air_density_kg_m3?, vacuum?, low_g?: "governor" | "physical" }` over Earth; `flight` is `air_time` or
`clamped`. `input` is `{ throttle?, strafe?, steer?, hop?, jump? }`, held for every step. `floor` is a flat height;
`floor_profile` is a `HeightGrid` `{ origin: [x, z], cell, cols, rows, heights }` (the index convention above); neither
is a flat floor at 0. Without `state` the hull starts at rest at the origin. `steps` is 1 (default) to 100000; `dt` is
1/60 s by default, above 0 and at most 0.1. The result's `state` (`{ pos, ...HoverState }`) is the next call's `state`:
two calls of 60 steps equal one of 120, bit for bit (`digest` is a 64-bit FNV-1a of every word).

```json
{ "method": "detsim.hover_step", "params": { "input": { "throttle": 1.0, "steer": 0.25 }, "steps": 60 } }
```

`detsim.hover_probe` measures a hull's feel by a scripted fixed-step run (`regia_framework::det_hover::probe`):

| `script` | Floor | Drive | `numbers` |
|---|---|---|---|
| `accel` | flat | full throttle from rest for 30 s | `top_speed_m_s` (highest forward speed), `time_to_90_s` (first time at 90 % of it) |
| `brake` | flat | `accel`, then throttle -1 until the forward speed is 0 (at most 30 s) | `top_speed_m_s`, `stop_distance_m`, `stop_time_s` |
| `turn` | flat | to half the top speed, then full steer for 6 s holding that speed (throttle 1 below it, 0 above) | `top_speed_m_s`, `turn_radius_m` (mean speed / mean yaw rate over the last 2 s), `turn_speed_m_s` |
| `hop` | flat | at rest, one hop on the first tick | `hop_apex_m` (above the ride height), `hop_air_s` |
| `crest` | a 1:5 rise from z = 0 to a 6 m peak at z = 30, a 1:5 fall to 0 at z = 60 | full throttle from rest at z = -60 for 30 s | `crest_air_s` (time in the air after the peak), `crest_peak_m` (highest altitude above the floor then) |

A number the run never reached is left out. Two calls give the same numbers (`hover_probe_numbers_are_stable`).

The default hull (`DetHoverHull::DEFAULT`, `WorldPhysics::EARTH`, air-time rules, 1/60 s), from
`hover_probe_numbers_are_stable` (`cargo test -p regia_framework --lib hover_probe_numbers_are_stable -- --nocapture`
prints a `regia-hover-probe` line per script; x86-64 Linux, 2026-11-18):

| Script | Numbers |
|---|---|
| `accel` | top speed 30.0 m/s, 90 % of it after 2.32 s |
| `brake` | stops in 17.06 m and 1.17 s from 30 m/s |
| `turn` | radius 7.51 m at a mean 11.27 m/s (the hull sheds speed in a full-lock turn; the hold only adds throttle below 15 m/s) |
| `hop` | apex 3.00 m above the ride height (`HoverLimits::DEFAULT.hop_apex_m`), 1.53 s in the air |
| `crest` | 2.58 s in the air after the peak, at most 14.21 m above the floor |

**Error codes.**

| Code | When |
|---|---|
| `hover_bad_params` | A `params`, `limits`, `world`, `flight` or footprint field is invalid or unknown. `data.field` names it (`ride_height`, `world.gravity_m_s2`, `half_length`, ...) and `data.reason` is its code: `not_finite`, `negative`, `zero_rate`, `out_of_range`, `inverted_range`, `negative_zero`, `unknown_field` or `wrong_type`. |
| `hover_bad_input` | `input` is missing or not `{ throttle?, strafe?, steer?, hop?, jump? }`; a number is not a finite f32; `state`, `floor` or `floor_profile` is malformed (or both floors are sent); `dt` is outside (0, 0.1]; `script` is not one of the five. |
| `hover_no_hull` | The entity has no `DetHoverHull` (`detsim.hover_get`, `detsim.hover_set_input`). |
| `hover_steps_limit` | `steps` is not a whole number from 1 to 100000 (`detsim.hover_step`). |

Checks: `cargo test -p regia_framework --lib det_hover` (`det_hover_system_matches_the_fleet`,
`det_hover_ignores_frame_rate`, `det_hover_rollback_replays_bit_for_bit`, `hover_probe_numbers_are_stable`,
`transforms_and_lean_are_written_and_never_read_back`, `hop_and_jump_fire_from_the_input`,
`hulls_without_an_id_wait_and_shared_ids_are_counted`, `the_terrain_floor_is_the_terrain_sampler`), and
`cargo test -p regia_agent --lib det_hover` (the five methods and every error code through the dispatcher).

## Hull files

A hover hull can be shipped as a `.hoverhull.ron` file (format `hoverhull`, version 1, ADR-8512,
`docs/formats/hoverhull.md`). The host parses the text with
`regia_framework::det_hover::HoverHullAsset::from_ron`. A lockstep match does not compare the text.
It compares the hull handshake (ADR-8632), `HoverHullParts::handshake_bytes(host_limits,
host_flight)`, built by `regia_detsim::hover::hull_handshake_bytes(params, limits, flight)`: 273
bytes, everything the hull step reads from the file.

| Bytes | Content |
|---|---|
| 0..180 | `params`: `HoverParams::to_le_bytes()`, four little-endian raw `f32` bits per `HoverParams::FIELD_RANGES` field, in that order |
| 180 | `flight`: 0 `air_time`, 1 `clamped` |
| 181..273 | `limits`: four little-endian raw `f32` bits per `HoverLimits::FIELD_RANGES` field, in that order |

An omitted `limits` block or `flight` is filled with what the host steps (`host_limits`,
`host_flight`) before the bytes are written. `-0.0` stays `-0.0`, and two hulls one ulp apart
differ in one word. A float token that two hosts parse into different bits, or a peer whose file
has other limits or another flight mode, cannot desync: each peer compares the bytes before the
first tick and refuses a peer whose bytes differ. `hull_handshake_mismatch(local, peer)` is `None`
for the same bytes, otherwise the first field that differs (`HullHandshakeMismatch::Params(name)`,
`Flight`, `Limits(name)`, `Extension { offset }` for bytes a format appended, `Length`); a host
reports it with the code `HullHandshakeMismatch::CODE`, `handshake_mismatch` (a library code, not a
`detsim.hover_hull_check` error). The world (`WorldPhysics::to_le_bytes()`) and a `ClimbTuning`
that is not a `FlightMode::climb` are not in these bytes: a host compares them itself. A format
built on a hull (a bike file) appends its own bytes after the 273 and compares the whole.

Every `params` and `limits` field is optional. A missing field takes `HoverParams::DEFAULT_HULL` or
`HoverLimits::DEFAULT`, not `0.0`. An omitted `limits` block means the host keeps its own limits.
`flight` is a `FlightMode` name (`air_time` or `clamped`); omitted means the host's mode. `notes`
is free text the step does not read. The loader accepts named fields only (a positional tuple is
`parse_error`) and RON `implicit_some`, so `ride_height: 1.5` and `Some(1.5)` are the same, and a
`limits` block may be `(...)` or `Some((...))`. A file may also start with `#![enable(implicit_some)]`.

`regia upgrade-project` knows the suffix `.hoverhull.ron`. Version 1 is the only version, so there
is nothing to migrate. Another `format_version` is `bad_version`.

### `detsim.hover_hull_check`

`detsim.hover_hull_check` takes exactly one of `text` (the file's text, at most 1 MiB) or `path`
(a file under the project, the same confinement as other agent paths, at most 1 MiB). It answers
`ok`, `name`, `format_version`, `flight`, `notes`, `limits`, `params`, `handshake_hex` and
`bytes_hex`. `handshake_hex` is the 273 handshake bytes as hex, with an omitted `limits` block or
`flight` filled with the engine defaults (`HoverLimits::DEFAULT`, `air_time`); compare it.
`bytes_hex` is the 180 `params` bytes only: two files that differ only in `limits` or `flight` have
the same `bytes_hex` and different `handshake_hex`. JSON numbers are not the handshake: JSON cannot
keep `-0.0`.

| Code | Meaning |
|---|---|
| `parse_error` | The text is not a named `.hoverhull.ron`, the file cannot be read, or it is not UTF-8. |
| `bad_version` | `format_version` is not 1. |
| `bad_name` | `name` is empty or only whitespace. |
| `bad_flight` | `flight` is not `air_time` or `clamped`. |
| `not_finite` | A field is NaN or infinite. |
| `negative` | Below 0 where the range starts at 0. |
| `zero_rate` | 0 for a rate or time constant that must be above 0. |
| `out_of_range` | Outside the field's documented range. |
| `inverted_range` | `yaw_deadband` above `yaw_rate_at_rest`. |

A field error names `field` (a limits field is `limits.<field>`). The call's JSON-RPC code stays
`F0002`; `format_code` is the code in this table. Giving both `text` and `path`, or neither, is
`F0002` with no `format_code`.
