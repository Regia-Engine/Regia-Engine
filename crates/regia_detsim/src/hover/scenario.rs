// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/trace_harness.rs, commit d6ed37677), ADR-8012.
// Mark: as-is for the loop of `HoverLibm1000::step` (DO27's `run_libm_1000`, lines 80-97: one hover step at full
// throttle on flat ground, then XZ += velocity * dt and Y = floor + altitude). The scenario wrapper and
// `HoverRateModel` are Regia's (ADR-8046's `DetScenario` and `RateModel` traits). The position update of that loop is
// now spelled as `host::integrate` (the same three statements, ADR-8508); `HoverFleetTerrain` is Regia's.

//! The hover step as an F-02 golden scenario and as a rate-gate model (ADR-8079).

use crate::golden::rate_gate::RateModel;
use crate::golden::{DetRun, DetScenario, StateHasher};
use crate::tick::TickRate;

use crate::ballistic::{self, LowG};
use crate::float::det_clamp;
use crate::golden::scenarios::SplitMix64;
use crate::lowg::{self, ArcRow};
use crate::math;

use super::host::{FleetHull, FleetInput, FleetReport, FloorSampler, HoverBody, HoverFleet, integrate};
use super::{
    ClimbTuning, FlightMode, HoverLimits, HoverParams, HoverState, WorldPhysics, apply_jump, step,
    try_step_tuned,
};

/// The libm-1000 cross-OS vector as a [`DetScenario`] (`hover_libm_1000`): the engine's default hull
/// ([`HoverParams::DEFAULT_HULL`], or the hull given to [`HoverLibm1000::with_params`]) at full
/// throttle from rest on flat ground at height 0, 1000 ticks at 60 Hz. The caller-side pose
/// integrates `velocity * dt` in XZ and sets Y to `floor + altitude`, as DO27's vector of the same
/// name does, so with the default hull the final pose and velocity are the six words of the
/// committed `hash=` line (`tests/fixtures/hover/libm-1000.txt`). A game runs the same vector on its
/// own hull with [`HoverLibm1000::with_params`] and pins that hash in its own repository. The golden hashes the
/// pose and every [`HoverState`] word after each tick, so a drift names its first tick and field.
/// The seed is unused (the input is fixed).
#[derive(Debug, Clone)]
pub struct HoverLibm1000 {
    params: HoverParams,
    body: HoverBody,
    dt: f32,
}

impl HoverLibm1000 {
    /// Floor height under the hull, metres.
    pub const FLOOR_Y: f32 = 0.0;
    /// Input `[throttle, strafe, steer]`.
    pub const INPUT: [f32; 3] = [1.0, 0.0, 0.0];

    /// The scenario on [`HoverParams::DEFAULT_HULL`], at rest, reset for its default run.
    pub fn new() -> Self {
        HoverLibm1000::with_params(HoverParams::DEFAULT_HULL)
    }

    /// The scenario on `params` (a game's own hull), at rest, reset for its default run. Only the
    /// default hull has a committed golden in this crate.
    pub fn with_params(params: HoverParams) -> Self {
        let mut s = HoverLibm1000 {
            params,
            body: HoverBody::new([0.0; 3]),
            dt: TickRate::DEFAULT.dt(),
        };
        s.reset(&s.default_run());
        s
    }

    /// World position `[x, y, z]` after the last step.
    pub fn pose(&self) -> [f32; 3] {
        self.body.pos
    }

    /// Hover state after the last step.
    pub fn state(&self) -> &HoverState {
        &self.body.state
    }
}

impl Default for HoverLibm1000 {
    fn default() -> Self {
        HoverLibm1000::new()
    }
}

impl DetScenario for HoverLibm1000 {
    fn name(&self) -> &str {
        "hover_libm_1000"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0, 1000, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.body = HoverBody::new([0.0; 3]);
        self.dt = run.rate.dt();
    }

    fn step(&mut self, _tick: u32) {
        let dt = self.dt;
        let floor_y = Self::FLOOR_Y;
        step(&self.params, &mut self.body.state, &Self::INPUT, floor_y, dt);
        // jump stays false — no apply_jump
        // XZ += velocity * dt, then Y = floor + altitude: the host kit's `integrate`, bit for bit.
        integrate(&mut self.body, dt, floor_y);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        let s = &self.body.state;
        h.f32("pos_x", self.body.pos[0]);
        h.f32("pos_y", self.body.pos[1]);
        h.f32("pos_z", self.body.pos[2]);
        h.f32("vel_x", s.velocity[0]);
        h.f32("vel_y", s.velocity[1]);
        h.f32("vel_z", s.velocity[2]);
        h.f32("vertical_vel", s.vertical_vel);
        h.f32("yaw", s.yaw);
        h.f32("yaw_rate", s.yaw_rate);
        h.bool("airborne", s.airborne);
        h.bool("jump_active", s.jump_active);
        h.f32("altitude", s.altitude);
        h.bool("prev_terrain_set", s.prev_terrain.is_some());
        h.f32("prev_terrain", s.prev_terrain.unwrap_or(0.0));
        h.f32("hop_cooldown", s.hop_cooldown);
        h.f32("ground_climb", s.ground_climb);
        h.f32("slope", s.slope);
        h.f32("roll", s.roll);
        h.f32("roll_rate", s.roll_rate);
        h.f32("pitch", s.pitch);
        h.bool("level_off", s.level_off);
        h.bool("yaw_modern", s.yaw_modern);
        h.f32("coast_drop_left", s.coast_drop_left);
        h.bool("thrusting", s.thrusting);
        h.f32("ground_pitch", s.ground_pitch);
        h.f32("ground_roll", s.ground_roll);
        h.f32("cushion_pitch", s.cushion_pitch);
        h.f32("cushion_pitch_rate", s.cushion_pitch_rate);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        self.body.pos[0] = f32::from_bits(self.body.pos[0].to_bits() ^ (1 << (bit % 32)));
        true
    }
}

/// The bad-input guards as a [`DetScenario`] (`hover_bad_input`, ADR-8309): the default hull
/// driven by seeded random input over a rolling floor with ledges, while the run feeds it every kind
/// of bad value [`try_step_tuned`] guards against: a NaN or infinite input axis every 7th tick, a
/// NaN or infinite floor every 11th, a `dt` of 0, `-dt`, NaN or infinity every 50th, and an infinite
/// roll rate written into the state every 200th (a recovery). It flies the air-time rules for the
/// first 300 ticks and the clamped rules after, with a jump every 150 ticks. The golden hashes the
/// pose, every [`HoverState`] word and the recovery count, so a target whose guards take another
/// branch shows up as a diverging tick.
#[derive(Debug, Clone)]
pub struct HoverBadInput {
    params: HoverParams,
    state: HoverState,
    pose: [f32; 3],
    dt: f32,
    rng: SplitMix64,
    floor: f32,
    recoveries: u32,
}

impl HoverBadInput {
    /// The values injected in turn: NaN of both signs and both infinities.
    pub const POISONS: [f32; 4] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -f32::NAN];

    /// The scenario on [`HoverParams::DEFAULT_HULL`], reset for its default run.
    pub fn new() -> Self {
        let mut s = HoverBadInput {
            params: HoverParams::DEFAULT_HULL,
            state: HoverState::default(),
            pose: [0.0; 3],
            dt: TickRate::DEFAULT.dt(),
            rng: SplitMix64::new(0),
            floor: 0.0,
            recoveries: 0,
        };
        s.reset(&s.default_run());
        s
    }

    /// Hover state after the last step.
    pub fn state(&self) -> &HoverState {
        &self.state
    }

    /// Ticks so far that reset a poisoned state.
    pub fn recoveries(&self) -> u32 {
        self.recoveries
    }
}

impl Default for HoverBadInput {
    fn default() -> Self {
        HoverBadInput::new()
    }
}

impl DetScenario for HoverBadInput {
    fn name(&self) -> &str {
        "hover_bad_input"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x0BAD_1A9E, 600, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.state = HoverState {
            altitude: self.params.ride_height,
            ..HoverState::default()
        };
        self.pose = [0.0, self.params.ride_height, 0.0];
        self.dt = run.rate.dt();
        self.rng = SplitMix64::new(run.seed);
        self.floor = 0.0;
        self.recoveries = 0;
    }

    fn step(&mut self, tick: u32) {
        let poison = |n: u32| Self::POISONS[(n % 4) as usize];
        let mut input = [self.rng.signed(), self.rng.signed(), self.rng.signed()];
        self.floor += 0.05 * self.rng.signed();
        if tick % 97 == 13 {
            self.floor += 2.0;
        }
        let mut floor = self.floor;
        let mut dt = self.dt;
        if tick % 7 == 6 {
            input[((tick / 7) % 3) as usize] = poison(tick / 7);
        }
        if tick % 11 == 10 {
            floor = poison(tick / 11);
        }
        if tick % 50 == 49 {
            dt = [0.0, -self.dt, f32::NAN, f32::INFINITY][((tick / 50) % 4) as usize];
        }
        if tick % 200 == 150 {
            self.state.roll_rate = f32::INFINITY;
        }
        if tick % 150 == 40 {
            apply_jump(&mut self.state, &self.params);
        }
        let climb = if tick < 300 {
            &ClimbTuning::AIR_TIME
        } else {
            &ClimbTuning::CLAMPED_1014
        };
        let report = try_step_tuned(&self.params, &mut self.state, &input, floor, dt, 9.8, 0.0, climb);
        if report.recovered {
            self.recoveries += 1;
        }
        // The host's side: the floor the step used, and the position moved by the tick's own dt.
        self.pose[0] += self.state.velocity[0] * self.dt;
        self.pose[2] += self.state.velocity[2] * self.dt;
        self.pose[1] = self.state.prev_terrain.unwrap_or(0.0) + self.state.altitude;
    }

    fn hash_state(&self, h: &mut StateHasher) {
        let s = &self.state;
        h.f32("pos_x", self.pose[0]);
        h.f32("pos_y", self.pose[1]);
        h.f32("pos_z", self.pose[2]);
        h.f32("vel_x", s.velocity[0]);
        h.f32("vel_y", s.velocity[1]);
        h.f32("vel_z", s.velocity[2]);
        h.f32("vertical_vel", s.vertical_vel);
        h.f32("yaw", s.yaw);
        h.f32("yaw_rate", s.yaw_rate);
        h.bool("airborne", s.airborne);
        h.bool("jump_active", s.jump_active);
        h.f32("altitude", s.altitude);
        h.bool("prev_terrain_set", s.prev_terrain.is_some());
        h.f32("prev_terrain", s.prev_terrain.unwrap_or(0.0));
        h.f32("hop_cooldown", s.hop_cooldown);
        h.f32("ground_climb", s.ground_climb);
        h.f32("slope", s.slope);
        h.f32("roll", s.roll);
        h.f32("roll_rate", s.roll_rate);
        h.f32("pitch", s.pitch);
        h.bool("level_off", s.level_off);
        h.bool("yaw_modern", s.yaw_modern);
        h.f32("coast_drop_left", s.coast_drop_left);
        h.bool("thrusting", s.thrusting);
        h.f32("ground_pitch", s.ground_pitch);
        h.f32("ground_roll", s.ground_roll);
        h.f32("cushion_pitch", s.cushion_pitch);
        h.f32("cushion_pitch_rate", s.cushion_pitch_rate);
        h.u32("recoveries", self.recoveries);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        self.pose[0] = f32::from_bits(self.pose[0].to_bits() ^ (1 << (bit % 32)));
        true
    }
}

/// The hover step as a [`RateModel`] for the 60 Hz vs 120 Hz gates
/// ([`crate::golden::rate_gate::measure`]): one hull with the given parameters on flat ground at
/// height 0, stepped by [`step`].
#[derive(Debug, Clone)]
pub struct HoverRateModel {
    /// The hull's parameters.
    pub params: HoverParams,
    /// The hull's state.
    pub state: HoverState,
}

impl HoverRateModel {
    /// A hull with `params`, at rest.
    pub fn new(params: HoverParams) -> Self {
        HoverRateModel {
            params,
            state: HoverState::default(),
        }
    }
}

impl RateModel for HoverRateModel {
    fn reset(&mut self) {
        self.state = HoverState::default();
    }

    fn step(&mut self, input: [f32; 3], dt: f32) {
        step(&self.params, &mut self.state, &input, 0.0, dt);
    }

    fn velocity_xz(&self) -> [f32; 2] {
        [self.state.velocity[0], self.state.velocity[2]]
    }

    fn yaw(&self) -> f32 {
        self.state.yaw
    }
}

/// Floor profile of [`HoverMixed`] along x, metres, for `x` in `[-100, 100)`: flat ends (so the
/// wrapped course is continuous), a 0.2 grade ramp up to a 4 m crest at -40 and a 0.4 grade back
/// down, a 3 m ledge (up at -10, a 3 m drop at 0), a 51-degree face up to a plateau whose far edge
/// drops 6.17 m at 40, and a 5 m block from 60 to 70; both of the last steps exceed
/// [`HoverLimits::grade_teleport_m`] (4 m).
fn mixed_profile_x(x: f32) -> f32 {
    // tan(51 degrees).
    const FACE_GRADE: f32 = 1.234_897_2;
    if x < -60.0 {
        0.0
    } else if x < -40.0 {
        0.2 * (x + 60.0)
    } else if x < -30.0 {
        4.0 - 0.4 * (x + 40.0)
    } else if x < -10.0 {
        0.0
    } else if x < 0.0 {
        3.0
    } else if x < 20.0 {
        0.0
    } else if x < 25.0 {
        FACE_GRADE * (x - 20.0)
    } else if x < 40.0 {
        FACE_GRADE * 5.0
    } else if (60.0..70.0).contains(&x) {
        5.0
    } else {
        0.0
    }
}

/// Floor profile of [`HoverMixed`] along z, metres, for `z` in `[-100, 100)`: rolling 1.5 m ridges
/// every 50 m (0.06 grade up, then down), so every heading meets a crest.
fn mixed_profile_z(z: f32) -> f32 {
    let mut u = z + 100.0;
    while u >= 50.0 {
        u -= 50.0;
    }
    while u < 0.0 {
        u += 50.0;
    }
    let t = if u < 25.0 { u } else { 50.0 - u };
    0.06 * t
}

/// The floor height of [`HoverMixed`] at `(x, z)`, both in `[-100, 100)`.
pub fn mixed_floor(x: f32, z: f32) -> f32 {
    mixed_profile_x(x) + mixed_profile_z(z)
}

/// Wrap a coordinate of the [`HoverMixed`] course into `[-100, 100)` (the floor is periodic).
fn mixed_wrap(v: f32) -> f32 {
    let mut v = v;
    while v >= 100.0 {
        v -= 200.0;
    }
    while v < -100.0 {
        v += 200.0;
    }
    v
}

/// What the [`HoverMixed`] run went through, counted per run (reset with the scenario): the guard
/// that a later edit of the scenario still reaches every branch it was written for. Each count is
/// summed over all hulls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoverMixedCoverage {
    /// Hull-ticks that ended airborne ([`HoverState::airborne`]).
    pub airborne: u32,
    /// Hull-ticks that ended on the cushion (not airborne).
    pub cushion: u32,
    /// Hull-ticks stepped with [`FlightMode::AirTime`].
    pub air_time: u32,
    /// Hull-ticks stepped with [`FlightMode::Clamped`].
    pub clamped: u32,
    /// Hop presses that fired ([`super::apply_hull_hop_with`] returned true).
    pub hops_fired: u32,
    /// Hop presses refused because the hop cooldown was still running.
    pub hops_refused: u32,
    /// Jumps that launched ([`apply_jump`] on a hull that was not airborne).
    pub jumps: u32,
    /// Hull-ticks that ended on the cushion, above the ride height, over falling ground
    /// (`ground_climb < 0`): the crest-fall rules.
    pub crest_falls: u32,
    /// Hull-ticks with a non-zero steer inside the hull's yaw deadband at full throttle (the
    /// commanded yaw rate is 0).
    pub deadband_hits: u32,
    /// Hull-ticks whose floor moved by at least [`HoverLimits::grade_teleport_m`] since the last tick.
    pub teleport_steps: u32,
    /// Hull-ticks stepped in the air with auto-level off ([`HoverState::level_off`]): the free roll
    /// and the thrust along the hull's tilted axes.
    pub free_air: u32,
    /// Hull-ticks stepped with the modern aim preset ([`HoverState::yaw_modern`]).
    pub yaw_modern: u32,
    /// Hull-ticks airborne in a jump on a world whose gravity is below
    /// [`HoverLimits::jump_feel_gravity_min`] (the low-g jump gravity floor).
    pub low_g_jumps: u32,
}

impl HoverMixedCoverage {
    /// Every counter with its name, in declaration order.
    pub fn named(&self) -> [(&'static str, u32); 13] {
        [
            ("airborne", self.airborne),
            ("cushion", self.cushion),
            ("air_time", self.air_time),
            ("clamped", self.clamped),
            ("hops_fired", self.hops_fired),
            ("hops_refused", self.hops_refused),
            ("jumps", self.jumps),
            ("crest_falls", self.crest_falls),
            ("deadband_hits", self.deadband_hits),
            ("teleport_steps", self.teleport_steps),
            ("free_air", self.free_air),
            ("yaw_modern", self.yaw_modern),
            ("low_g_jumps", self.low_g_jumps),
        ]
    }
}

/// One hull of [`HoverMixed`].
#[derive(Debug, Clone, Copy)]
struct MixedHull {
    params: HoverParams,
    world: WorldPhysics,
    hop_speed: f32,
    rng: SplitMix64,
    state: HoverState,
    pose: [f32; 3],
    floor: f32,
    input: [f32; 3],
    mode: FlightMode,
}

/// The mixed cross-target hover vector as a [`DetScenario`] (`hover_mixed`): every rule of the
/// hover step under seeded input, where `hover_libm_1000` only drives straight on flat ground.
///
/// Twelve hulls step side by side, one per pair of [`HoverMixed::WORLDS`] (gravity and air:
/// Earth, a thin-air world, a thick-air low-g world and a near-airless asteroid) and
/// [`HoverMixed::HULLS`] (the default hull, a light skiff without deadband and a heavy barge), for
/// 3600 ticks at 60 Hz. Each hull draws its input from its own seeded stream, held 15 ticks: `-0.0`,
/// `+0.0`, `+-1`, `+-1.5` (clamped), the 0.05 steer threshold and its neighbor, the hull's
/// deadband edge and half of it, and uniform values; with it a mouse pitch. Every 450 ticks the
/// flight mode, auto-level and modern-yaw flags step through all eight combinations (each hull from
/// another phase). Hops are pressed on a 240-tick schedule (one fires, one is refused by the
/// cooldown, one meets a hull in the air) and jumps every 150 ticks. The floor ([`mixed_floor`]) has
/// ramps, crests, a 3 m drop, a 51-degree face and steps larger than
/// [`HoverLimits::grade_teleport_m`]; the host's ground pass writes the ground pitch and roll from
/// floor samples around the hull, and the caller-side pose wraps on the 200 m course.
///
/// The golden hashes, per hull, the pose, every [`HoverState`] word, the input and the floor, then
/// the [`HoverMixedCoverage`] counts, so a target whose float results or branches differ shows up as
/// a diverging tick and field.
#[derive(Debug, Clone)]
pub struct HoverMixed {
    hulls: [MixedHull; HoverMixed::HULL_COUNT],
    dt: f32,
    coverage: HoverMixedCoverage,
    input_flip: Option<(u32, u32)>,
}

impl HoverMixed {
    /// The worlds `(gravity, air)`: (9.81, 1.2), (3.71, 0.02), (1.35, 5.3) and (0.113, 0.0), the
    /// last in vacuum; low-g modes alternate (the hover step does not read them).
    pub const WORLDS: [WorldPhysics; 4] = [
        WorldPhysics {
            gravity_m_s2: 9.81,
            air_density_kg_m3: 1.2,
            vacuum: false,
            low_g: LowG::Governor,
        },
        WorldPhysics {
            gravity_m_s2: 3.71,
            air_density_kg_m3: 0.02,
            vacuum: false,
            low_g: LowG::Physical,
        },
        WorldPhysics {
            gravity_m_s2: 1.35,
            air_density_kg_m3: 5.3,
            vacuum: false,
            low_g: LowG::Governor,
        },
        WorldPhysics {
            gravity_m_s2: 0.113,
            air_density_kg_m3: 0.0,
            vacuum: true,
            low_g: LowG::Physical,
        },
    ];

    /// The hulls: [`HoverParams::DEFAULT_HULL`], a light fast skiff (no yaw deadband, lateral drag,
    /// a release yaw response and high-speed drag) and a heavy barge (a wide deadband, a soft spring,
    /// no release drop, no turn bleed), both derived from the default hull.
    pub const HULLS: [HoverParams; 3] = [
        HoverParams::DEFAULT_HULL,
        HoverParams {
            ride_height: 0.6,
            thrust_accel: 22.0,
            max_forward_speed: 40.0,
            max_strafe_speed: 20.0,
            yaw_deadband: 0.0,
            lateral_drag: 1.5,
            yaw_release_response: 6.0,
            accel_drag_high: 6.0,
            drag_knee: 25.0,
            jump_speed: 7.0,
            hover_lead: 0.6,
            ..HoverParams::DEFAULT_HULL
        },
        HoverParams {
            ride_height: 1.8,
            thrust_accel: 8.0,
            max_forward_speed: 18.0,
            brake_accel: 12.0,
            yaw_rate_at_rest: 1.2,
            yaw_rate_at_speed: 0.8,
            yaw_deadband: 0.4,
            yaw_response_at_speed: 0.0,
            turn_bleed: 0.0,
            coast_drop: 0.0,
            coast_drop_share: 0.0,
            height_spring: 12.0,
            height_damper: 6.0,
            jump_speed: 14.0,
            ..HoverParams::DEFAULT_HULL
        },
    ];

    /// Hulls stepped per tick: one per world and hull.
    pub const HULL_COUNT: usize = 12;
    /// Ticks an input is held.
    pub const INPUT_HOLD: u32 = 15;
    /// Ticks between flight-mode, auto-level and modern-yaw changes.
    pub const MODE_PERIOD: u32 = 450;
    /// Ticks between hop schedules: a press at 0 (fires), 25 (refused by the cooldown) and 61.
    pub const HOP_PERIOD: u32 = 240;

    /// The scenario, reset for its default run.
    pub fn new() -> Self {
        let blank = MixedHull {
            params: HoverParams::DEFAULT_HULL,
            world: Self::WORLDS[0],
            hop_speed: 0.0,
            rng: SplitMix64::new(0),
            state: HoverState::default(),
            pose: [0.0; 3],
            floor: 0.0,
            input: [0.0; 3],
            mode: FlightMode::AirTime,
        };
        let mut s = HoverMixed {
            hulls: [blank; Self::HULL_COUNT],
            dt: TickRate::DEFAULT.dt(),
            coverage: HoverMixedCoverage::default(),
            input_flip: None,
        };
        s.reset(&s.default_run());
        s
    }

    /// A fault-injection control: flip bit `bit` (0-31) of hull 0's throttle input at tick `tick`,
    /// before the step reads it. The input is hashed, so the run first differs at `tick`.
    pub fn with_input_flip(mut self, tick: u32, bit: u32) -> Self {
        self.input_flip = Some((tick, bit % 32));
        self
    }

    /// What the run went through since the last reset.
    pub fn coverage(&self) -> HoverMixedCoverage {
        self.coverage
    }

    /// Hover state of hull `index` (`world * 3 + hull`), if there is one.
    pub fn state(&self, index: usize) -> Option<&HoverState> {
        self.hulls.get(index).map(|h| &h.state)
    }

    /// One input axis from `rng`: a special value (signed zeros, saturation, over-range, the 0.05
    /// steer threshold, the deadband edge) or a uniform draw in `[lo, lo + span)`.
    fn draw_axis(rng: &mut SplitMix64, deadband_edge: f32, lo: f32, span: f32) -> f32 {
        match rng.next_u64() % 16 {
            0 => -0.0,
            1 => 0.0,
            2 => 1.0,
            3 => -1.0,
            4 => 1.5,
            5 => -1.5,
            6 => 0.05,
            7 => -0.049_999_997,
            8 => deadband_edge,
            9 => -0.5 * deadband_edge,
            _ => lo + rng.unit() * span,
        }
    }
}

impl Default for HoverMixed {
    fn default() -> Self {
        HoverMixed::new()
    }
}

impl DetScenario for HoverMixed {
    fn name(&self) -> &str {
        "hover_mixed"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_4d1e, 3600, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.dt = run.rate.dt();
        self.coverage = HoverMixedCoverage::default();
        let limits = HoverLimits::DEFAULT;
        for (i, hull) in self.hulls.iter_mut().enumerate() {
            let params = Self::HULLS[i % 3];
            let world = Self::WORLDS[i / 3];
            let n = i as f32;
            let x = mixed_wrap(-100.0 + 17.0 * n);
            let z = mixed_wrap(-95.0 + 23.0 * n);
            let floor = mixed_floor(x, z);
            *hull = MixedHull {
                params,
                world,
                hop_speed: super::hop_launch_speed_for(&limits, &world, self.dt),
                rng: SplitMix64::new(run.seed ^ (0x9e37_79b9_u64.wrapping_mul(i as u64 + 1))),
                state: HoverState {
                    altitude: params.ride_height,
                    prev_terrain: Some(floor),
                    yaw: 0.5 * n - 3.0,
                    ..HoverState::default()
                },
                pose: [x, floor + params.ride_height, z],
                floor,
                input: [0.0; 3],
                mode: FlightMode::AirTime,
            };
        }
    }

    fn step(&mut self, tick: u32) {
        let dt = self.dt;
        let limits = HoverLimits::DEFAULT;
        let segment = tick / Self::MODE_PERIOD;
        for (i, hull) in self.hulls.iter_mut().enumerate() {
            let iu = i as u32;
            let params = hull.params;
            // The host's input: held, then redrawn on this hull's phase.
            if tick % Self::INPUT_HOLD == iu % Self::INPUT_HOLD {
                let edge = if params.yaw_rate_at_rest > 0.0 {
                    params.yaw_deadband / params.yaw_rate_at_rest
                } else {
                    0.0
                };
                let throttle = HoverMixed::draw_axis(&mut hull.rng, edge, -0.4, 1.4);
                let strafe = HoverMixed::draw_axis(&mut hull.rng, edge, -1.0, 2.0);
                let steer = HoverMixed::draw_axis(&mut hull.rng, edge, -1.0, 2.0);
                hull.input = [throttle, strafe, steer];
                hull.state.pitch = 0.35 * hull.rng.signed();
            }
            let mut input = hull.input;
            if let Some((flip_tick, bit)) = self.input_flip
                && i == 0
                && flip_tick == tick
            {
                input[0] = f32::from_bits(input[0].to_bits() ^ (1 << bit));
                hull.input = input;
            }
            // The match settings, carried on the input every tick: all eight combinations per hull.
            let phase = segment + iu;
            hull.mode = if phase.is_multiple_of(2) {
                FlightMode::AirTime
            } else {
                FlightMode::Clamped
            };
            hull.state.level_off = (segment / 2 + iu) % 2 == 1;
            hull.state.yaw_modern = (segment / 4 + iu / 2) % 2 == 1;
            // Hops and jumps on a schedule.
            let hop_phase = (tick + 7 * iu) % Self::HOP_PERIOD;
            if hop_phase == 0 || hop_phase == 25 || hop_phase == 61 {
                let refused = hull.state.hop_cooldown > 0.0;
                if super::apply_hull_hop_with(&limits, &mut hull.state, hull.hop_speed) {
                    self.coverage.hops_fired += 1;
                } else if refused {
                    self.coverage.hops_refused += 1;
                }
            }
            if (tick + 13 * iu) % 150 == 75 && !hull.state.airborne {
                apply_jump(&mut hull.state, &params);
                self.coverage.jumps += 1;
            }
            // The host's ground pass: the floor under the hull and its pitch and roll from samples
            // a metre ahead, behind, right and left.
            let [x, _, z] = hull.pose;
            let floor = mixed_floor(x, z);
            let (s, c) = (math::sin(hull.state.yaw), math::cos(hull.state.yaw));
            let ahead = mixed_floor(mixed_wrap(x + s), mixed_wrap(z + c));
            let behind = mixed_floor(mixed_wrap(x - s), mixed_wrap(z - c));
            let right = mixed_floor(mixed_wrap(x + c), mixed_wrap(z - s));
            let left = mixed_floor(mixed_wrap(x - c), mixed_wrap(z + s));
            hull.state.ground_pitch = math::atan2(ahead - behind, 2.0);
            hull.state.ground_roll = math::atan2(right - left, 2.0);
            if (floor - hull.floor).abs() >= limits.grade_teleport_m {
                self.coverage.teleport_steps += 1;
            }
            hull.floor = floor;

            // Coverage read before the step, from what the step will see.
            let throttle = det_clamp(input[0], -1.0, 1.0);
            let steer = det_clamp(input[2], -1.0, 1.0);
            if params.yaw_deadband > 0.0
                && steer != 0.0
                && throttle.abs() >= 1.0
                && steer.abs() * params.yaw_rate_at_rest <= params.yaw_deadband
            {
                self.coverage.deadband_hits += 1;
            }
            if hull.state.level_off && limits.hull_in_air(&params, &hull.state) {
                self.coverage.free_air += 1;
            }
            if hull.state.yaw_modern {
                self.coverage.yaw_modern += 1;
            }
            match hull.mode {
                FlightMode::AirTime => self.coverage.air_time += 1,
                FlightMode::Clamped => self.coverage.clamped += 1,
            }
            let was_airborne = hull.state.airborne;

            super::step_with(
                &params,
                &limits,
                hull.mode.climb(),
                &hull.world,
                &mut hull.state,
                &input,
                floor,
                dt,
            );

            let st = &hull.state;
            if st.airborne {
                self.coverage.airborne += 1;
                if st.jump_active && hull.world.gravity_m_s2 < limits.jump_feel_gravity_min {
                    self.coverage.low_g_jumps += 1;
                }
            } else {
                self.coverage.cushion += 1;
            }
            if !was_airborne && !st.airborne && st.ground_climb < 0.0 && st.altitude > params.ride_height {
                self.coverage.crest_falls += 1;
            }
            // The caller's side: XZ by velocity, wrapped on the course; Y from the floor.
            hull.pose[0] = mixed_wrap(hull.pose[0] + st.velocity[0] * dt);
            hull.pose[2] = mixed_wrap(hull.pose[2] + st.velocity[2] * dt);
            hull.pose[1] = floor + st.altitude;
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, hull) in self.hulls.iter().enumerate() {
            let s = &hull.state;
            h.item("hull", i as u32);
            h.f32("pos_x", hull.pose[0]);
            h.f32("pos_y", hull.pose[1]);
            h.f32("pos_z", hull.pose[2]);
            h.f32("vel_x", s.velocity[0]);
            h.f32("vel_y", s.velocity[1]);
            h.f32("vel_z", s.velocity[2]);
            h.f32("vertical_vel", s.vertical_vel);
            h.f32("yaw", s.yaw);
            h.f32("yaw_rate", s.yaw_rate);
            h.bool("airborne", s.airborne);
            h.bool("jump_active", s.jump_active);
            h.f32("altitude", s.altitude);
            h.bool("prev_terrain_set", s.prev_terrain.is_some());
            h.f32("prev_terrain", s.prev_terrain.unwrap_or(0.0));
            h.f32("hop_cooldown", s.hop_cooldown);
            h.f32("ground_climb", s.ground_climb);
            h.f32("slope", s.slope);
            h.f32("roll", s.roll);
            h.f32("roll_rate", s.roll_rate);
            h.f32("pitch", s.pitch);
            h.bool("level_off", s.level_off);
            h.bool("yaw_modern", s.yaw_modern);
            h.f32("coast_drop_left", s.coast_drop_left);
            h.bool("thrusting", s.thrusting);
            h.f32("ground_pitch", s.ground_pitch);
            h.f32("ground_roll", s.ground_roll);
            h.f32("cushion_pitch", s.cushion_pitch);
            h.f32("cushion_pitch_rate", s.cushion_pitch_rate);
            h.f32("in_throttle", hull.input[0]);
            h.f32("in_strafe", hull.input[1]);
            h.f32("in_steer", hull.input[2]);
            h.f32("floor", hull.floor);
            h.bool("clamped_rules", hull.mode == FlightMode::Clamped);
            h.f32("hop_speed", hull.hop_speed);
        }
        h.item("coverage", 0);
        for (name, count) in self.coverage.named() {
            h.u32(name, count);
        }
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        let p = &mut self.hulls[0].pose[0];
        *p = f32::from_bits(p.to_bits() ^ (1 << (bit % 32)));
        true
    }
}

/// What the [`LowgBallisticMixed`] run went through, counted per run, summed over its cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LowgBallisticCoverage {
    /// Debris tosses launched (each landing relaunches one).
    pub tosses: u32,
    /// Tosses so fast that one tick of drag stops them (the drag cap of [`crate::ballistic::step_debris`]).
    pub capped_tosses: u32,
    /// Mortar shells fired (each landing fires another).
    pub shells: u32,
    /// Suit jumps launched (each landing launches another).
    pub jumps: u32,
}

/// One case of [`LowgBallisticMixed`]: one world in one low-g mode.
#[derive(Debug, Clone, Copy)]
struct LowgCase {
    world: WorldPhysics,
    hop_speed: f32,
    debris_pos: [f32; 3],
    debris_vel: [f32; 3],
    shell_pos: [f32; 3],
    shell_vel: [f32; 3],
    jumper_y: f32,
    jumper_vel: [f32; 3],
    apex: f32,
    row: ArcRow,
    jump_v: f32,
}

/// The low-g and ballistic rules as a [`DetScenario`] (`lowg_ballistic_mixed`): the worlds of
/// [`HoverMixed::WORLDS`], each in both [`LowG`] modes (eight cases), 3600 ticks at 60 Hz.
///
/// Per case and tick: a seeded suit apex (0.25 to 6.25 m) through [`crate::lowg::arc_row_for`] and
/// [`crate::lowg::jump_velocity`]; a debris toss stepped by [`crate::ballistic::step_debris`] with
/// the world's drag, relaunched with a seeded velocity on landing (every seventh toss in air is fast
/// enough to hit the drag cap); a mortar shell stepped per tick by
/// [`crate::ballistic::step_per_tick`] under the legacy mortar's gravity
/// ([`crate::ballistic::ArcLauncher::gravity`] of `ArcLauncher::LEGACY_MORTAR`) with the mode's
/// charge, refired on landing; and a suit jumper launched at the jump velocity under the jump-feel
/// gravity, relaunched on landing. Each case's hop launch speed ([`super::hop_launch_speed`]) is
/// computed once at reset and hashed. The golden hashes every word of every case and the
/// [`LowgBallisticCoverage`] counts.
#[derive(Debug, Clone)]
pub struct LowgBallisticMixed {
    cases: [LowgCase; LowgBallisticMixed::CASES],
    rng: SplitMix64,
    dt: f32,
    coverage: LowgBallisticCoverage,
}

impl LowgBallisticMixed {
    /// Cases stepped per tick: every world of [`HoverMixed::WORLDS`] in both low-g modes.
    pub const CASES: usize = 8;

    /// The scenario, reset for its default run.
    pub fn new() -> Self {
        let blank = LowgCase {
            world: HoverMixed::WORLDS[0],
            hop_speed: 0.0,
            debris_pos: [0.0; 3],
            debris_vel: [0.0; 3],
            shell_pos: [0.0; 3],
            shell_vel: [0.0; 3],
            jumper_y: 0.0,
            jumper_vel: [0.0; 3],
            apex: 0.0,
            row: ArcRow {
                apex_m: 0.0,
                hang_s: 0.0,
                landing_m_s: 0.0,
                mortar_range_m: 0.0,
            },
            jump_v: 0.0,
        };
        let mut s = LowgBallisticMixed {
            cases: [blank; Self::CASES],
            rng: SplitMix64::new(0),
            dt: TickRate::DEFAULT.dt(),
            coverage: LowgBallisticCoverage::default(),
        };
        s.reset(&s.default_run());
        s
    }

    /// What the run went through since the last reset.
    pub fn coverage(&self) -> LowgBallisticCoverage {
        self.coverage
    }
}

impl Default for LowgBallisticMixed {
    fn default() -> Self {
        LowgBallisticMixed::new()
    }
}

impl DetScenario for LowgBallisticMixed {
    fn name(&self) -> &str {
        "lowg_ballistic_mixed"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_1069, 3600, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.dt = run.rate.dt();
        self.rng = SplitMix64::new(run.seed);
        self.coverage = LowgBallisticCoverage::default();
        for (i, case) in self.cases.iter_mut().enumerate() {
            let world = WorldPhysics {
                low_g: if i.is_multiple_of(2) {
                    LowG::Governor
                } else {
                    LowG::Physical
                },
                ..HoverMixed::WORLDS[i / 2]
            };
            case.world = world;
            case.hop_speed = super::hop_launch_speed(world.gravity_m_s2, world.drag_air(), self.dt);
            case.debris_pos = [0.0; 3];
            case.debris_vel = [0.0; 3];
            case.shell_pos = [0.0; 3];
            case.shell_vel = [0.0; 3];
            case.jumper_y = 0.0;
            case.jumper_vel = [0.0; 3];
            case.apex = 0.0;
            case.row = lowg::arc_row_for(world.low_g, world.gravity_m_s2, 0.0);
            case.jump_v = 0.0;
        }
    }

    fn step(&mut self, _tick: u32) {
        let dt = self.dt;
        let rng = &mut self.rng;
        for case in &mut self.cases {
            let g = case.world.gravity_m_s2;
            let air = case.world.drag_air();
            let mode = case.world.low_g;
            case.apex = 0.25 + rng.unit() * 6.0;
            case.row = lowg::arc_row_for(mode, g, case.apex);
            case.jump_v = lowg::jump_velocity(case.apex, g, mode);

            // Debris: a seeded toss, relaunched on landing.
            case.debris_vel = ballistic::step_debris(case.debris_vel, g, air, dt);
            for (p, v) in case.debris_pos.iter_mut().zip(case.debris_vel) {
                *p += v * dt;
            }
            if case.debris_pos[1] <= 0.0 {
                case.debris_pos[1] = 0.0;
                self.coverage.tosses += 1;
                let capped = air > 0.0 && self.coverage.tosses % 7 == 6;
                let up = 2.0 + rng.unit() * 40.0;
                let (sx, sz) = (rng.signed() * 20.0, rng.signed() * 20.0);
                case.debris_vel = if capped {
                    self.coverage.capped_tosses += 1;
                    [9000.0, up, sz]
                } else {
                    [sx, up, sz]
                };
            }

            // Mortar: velocity per tick, mortar gravity, the mode's charge.
            case.shell_vel = ballistic::step_per_tick(
                case.shell_vel,
                ballistic::ArcLauncher::LEGACY_MORTAR.gravity(g),
                air,
                dt,
            );
            for (p, v) in case.shell_pos.iter_mut().zip(case.shell_vel) {
                *p += v;
            }
            if case.shell_pos[1] <= 0.0 {
                case.shell_pos = [0.0; 3];
                self.coverage.shells += 1;
                let v = ballistic::ArcLauncher::LEGACY_MORTAR.muzzle_m_s
                    * ballistic::charge_scale(mode, g)
                    * (0.5 + 0.5 * rng.unit());
                let (dx, dz) = (rng.signed(), rng.signed());
                let len = math::sqrt(dx * dx + dz * dz);
                let (dx, dz) = if len > 1.0e-3 {
                    (dx / len, dz / len)
                } else {
                    (1.0, 0.0)
                };
                case.shell_vel = [v * 0.9 * dx * dt, v * 0.4 * dt, v * 0.9 * dz * dt];
            }

            // Suit jumper: the jump velocity under the jump-feel gravity, relaunched on landing.
            case.jumper_vel = ballistic::step_debris(case.jumper_vel, super::jump_gravity(g), air, dt);
            case.jumper_y += case.jumper_vel[1] * dt;
            if case.jumper_y <= 0.0 {
                case.jumper_y = 0.0;
                self.coverage.jumps += 1;
                case.jumper_vel = [rng.signed() * 3.0, case.jump_v, rng.signed() * 3.0];
            }
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, case) in self.cases.iter().enumerate() {
            h.item("case", i as u32);
            h.f32("debris_x", case.debris_pos[0]);
            h.f32("debris_y", case.debris_pos[1]);
            h.f32("debris_z", case.debris_pos[2]);
            h.f32("debris_vx", case.debris_vel[0]);
            h.f32("debris_vy", case.debris_vel[1]);
            h.f32("debris_vz", case.debris_vel[2]);
            h.f32("shell_x", case.shell_pos[0]);
            h.f32("shell_y", case.shell_pos[1]);
            h.f32("shell_z", case.shell_pos[2]);
            h.f32("shell_vx", case.shell_vel[0]);
            h.f32("shell_vy", case.shell_vel[1]);
            h.f32("shell_vz", case.shell_vel[2]);
            h.f32("jumper_y", case.jumper_y);
            h.f32("jumper_vx", case.jumper_vel[0]);
            h.f32("jumper_vy", case.jumper_vel[1]);
            h.f32("jumper_vz", case.jumper_vel[2]);
            h.f32("apex", case.apex);
            h.f32("row_apex_m", case.row.apex_m);
            h.f32("row_hang_s", case.row.hang_s);
            h.f32("row_landing_m_s", case.row.landing_m_s);
            h.f32("row_mortar_range_m", case.row.mortar_range_m);
            h.f32("jump_v", case.jump_v);
            h.f32("hop_speed", case.hop_speed);
        }
        h.item("coverage", 0);
        h.u32("tosses", self.coverage.tosses);
        h.u32("capped_tosses", self.coverage.capped_tosses);
        h.u32("shells", self.coverage.shells);
        h.u32("jumps", self.coverage.jumps);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        let p = &mut self.cases[0].debris_pos[0];
        *p = f32::from_bits(p.to_bits() ^ (1 << (bit % 32)));
        true
    }
}

/// The procedural floor of [`HoverFleetTerrain`]: rolling hills on a gentle tilt toward +X,
/// `3 sin(0.045 x) + 2 cos(0.06 z) + 0.8 sin(0.13 (x + z)) + 0.02 x`, through [`crate::math`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RollingField;

impl FloorSampler for RollingField {
    fn floor(&self, x: f32, z: f32) -> f32 {
        3.0 * math::sin(0.045 * x) + 2.0 * math::cos(0.06 * z) + 0.8 * math::sin(0.13 * (x + z)) + 0.02 * x
    }
}

/// The host kit as a [`DetScenario`] (`hover_fleet_terrain`, ADR-8508): a [`HoverFleet`] of 64 hulls
/// of two types on an 8 x 8 grid over [`RollingField`], 1800 ticks at 60 Hz on
/// [`WorldPhysics::EARTH`]. Each hull draws new seeded axes every 20 ticks; the inputs are handed
/// over in descending slot order and every sixteenth hull gets none (neutral). Every 120 ticks one
/// hull jumps. The air-time rules fly the first 900 ticks, the clamped rules the rest. The golden
/// hashes every hull's position and every [`HoverState`] word (the ground attitude pass included), so
/// a target whose sampling, attitude or integration differs shows up as a diverging tick.
#[derive(Debug, Clone)]
pub struct HoverFleetTerrain {
    fleet: HoverFleet,
    held: Vec<[f32; 3]>,
    inputs: Vec<FleetInput>,
    rng: SplitMix64,
    dt: f32,
    report: FleetReport,
}

impl HoverFleetTerrain {
    /// Hulls in the fleet.
    pub const HULLS: u32 = 64;
    /// Ticks between new input draws.
    pub const INPUT_PERIOD: u32 = 20;

    /// The two hull types: the default hull (2 m x 1 m half footprint) and a taller, slower,
    /// longer one.
    pub fn hull_types() -> [FleetHull; 2] {
        let tall = HoverParams {
            ride_height: 1.4,
            max_forward_speed: 24.0,
            ..HoverParams::DEFAULT_HULL
        };
        [
            FleetHull::new(HoverParams::DEFAULT_HULL, 2.0, 1.0),
            FleetHull::new(tall, 2.6, 1.3),
        ]
    }

    /// The scenario reset for its default run.
    pub fn new() -> Self {
        let mut s = HoverFleetTerrain {
            fleet: HoverFleet::default(),
            held: Vec::with_capacity(Self::HULLS as usize),
            inputs: Vec::with_capacity(Self::HULLS as usize),
            rng: SplitMix64::new(0),
            dt: TickRate::DEFAULT.dt(),
            report: FleetReport::default(),
        };
        s.reset(&s.default_run());
        s
    }

    /// The fleet after the last step.
    pub fn fleet(&self) -> &HoverFleet {
        &self.fleet
    }

    /// The last tick's [`FleetReport`].
    pub fn report(&self) -> FleetReport {
        self.report
    }
}

impl Default for HoverFleetTerrain {
    fn default() -> Self {
        HoverFleetTerrain::new()
    }
}

impl DetScenario for HoverFleetTerrain {
    fn name(&self) -> &str {
        "hover_fleet_terrain"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0xF1EE_7064, 1800, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let types = Self::hull_types();
        self.fleet = HoverFleet::new(types.to_vec());
        for i in 0..Self::HULLS {
            let x = (i % 8) as f32 * 12.0 - 42.0;
            let z = (i / 8) as f32 * 12.0 - 42.0;
            let hull = u32::from(i.is_multiple_of(3));
            let params = &types[hull as usize].params;
            let yaw = i as f32 * 0.39 - 3.0;
            let floor = RollingField.floor(x, z);
            self.fleet
                .push(HoverBody::at_rest([x, floor, z], yaw, params), hull);
        }
        self.held.clear();
        self.held.resize(Self::HULLS as usize, [0.0; 3]);
        self.inputs.clear();
        self.rng = SplitMix64::new(run.seed);
        self.dt = run.rate.dt();
        self.report = FleetReport::default();
    }

    fn step(&mut self, tick: u32) {
        if tick.is_multiple_of(Self::INPUT_PERIOD) {
            for axes in &mut self.held {
                let throttle = self.rng.unit() * 1.3 - 0.3;
                let strafe = self.rng.signed() * 0.6;
                let steer = self.rng.signed();
                *axes = [throttle, strafe, steer];
            }
        }
        if tick % 120 == 60 {
            let slot = (tick / 120) % Self::HULLS;
            let hull = self.fleet.hull_of(slot).unwrap_or(0) as usize;
            let params = self.fleet.hulls()[hull].params;
            if let Some(body) = self.fleet.get_mut(slot) {
                apply_jump(&mut body.state, &params);
            }
        }
        self.inputs.clear();
        for slot in (0..Self::HULLS).rev() {
            if slot % 16 != 5 {
                self.inputs.push(FleetInput::new(slot, self.held[slot as usize]));
            }
        }
        let climb = if tick < 900 {
            &ClimbTuning::AIR_TIME
        } else {
            &ClimbTuning::CLAMPED_1014
        };
        self.report = self.fleet.step_all(
            &self.inputs,
            &RollingField,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            climb,
            self.dt,
        );
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, body) in self.fleet.bodies().iter().enumerate() {
            h.item("hull", i as u32);
            let s = &body.state;
            h.f32("pos_x", body.pos[0]);
            h.f32("pos_y", body.pos[1]);
            h.f32("pos_z", body.pos[2]);
            h.f32("vel_x", s.velocity[0]);
            h.f32("vel_y", s.velocity[1]);
            h.f32("vel_z", s.velocity[2]);
            h.f32("vertical_vel", s.vertical_vel);
            h.f32("yaw", s.yaw);
            h.f32("yaw_rate", s.yaw_rate);
            h.bool("airborne", s.airborne);
            h.bool("jump_active", s.jump_active);
            h.f32("altitude", s.altitude);
            h.bool("prev_terrain_set", s.prev_terrain.is_some());
            h.f32("prev_terrain", s.prev_terrain.unwrap_or(0.0));
            h.f32("hop_cooldown", s.hop_cooldown);
            h.f32("ground_climb", s.ground_climb);
            h.f32("slope", s.slope);
            h.f32("roll", s.roll);
            h.f32("roll_rate", s.roll_rate);
            h.f32("pitch", s.pitch);
            h.bool("level_off", s.level_off);
            h.bool("yaw_modern", s.yaw_modern);
            h.f32("coast_drop_left", s.coast_drop_left);
            h.bool("thrusting", s.thrusting);
            h.f32("ground_pitch", s.ground_pitch);
            h.f32("ground_roll", s.ground_roll);
            h.f32("cushion_pitch", s.cushion_pitch);
            h.f32("cushion_pitch_rate", s.cushion_pitch_rate);
        }
        h.u32("recovered", self.report.recovered);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.fleet.bodies_mut().first_mut() {
            Some(body) => {
                body.pos[0] = f32::from_bits(body.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}
