// SPDX-License-Identifier: MPL-2.0
//! The ground vehicles as an F-02 golden scenario (`ground_vehicle_mix`, ADR-8551).

use crate::ballistic::LowG;
use crate::golden::scenarios::SplitMix64;
use crate::golden::{DetRun, DetScenario, StateHasher};
use crate::hover::host::FloorSampler;
use crate::math;
use crate::site_physics::EARTH_G;
use crate::tick::TickRate;

use super::tracked::{self, TrackedInput, TrackedParams, TrackedState};
use super::wheeled::{self, WheeledInput, WheeledParams, WheeledState};
use super::{GroundReport, WorldPhysics};

/// The procedural floor of [`GroundVehicleMix`]: hills steep enough to reach the grade limits,
/// on a tilt toward +X, `4 sin(0.09 x) + 3 cos(0.08 z) + 1.5 sin(0.23 (x - z)) + 0.03 x`, through
/// [`crate::math`] (slopes up to about 40°).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GroundField;

impl FloorSampler for GroundField {
    fn floor(&self, x: f32, z: f32) -> f32 {
        4.0 * math::sin(0.09 * x) + 3.0 * math::cos(0.08 * z) + 1.5 * math::sin(0.23 * (x - z)) + 0.03 * x
    }
}

/// One world of the mix: its physics and its vehicles.
#[derive(Debug, Clone)]
struct MixWorld {
    world: WorldPhysics,
    tanks: Vec<TrackedState>,
    cars: Vec<WheeledState>,
    tank_inputs: Vec<TrackedInput>,
    car_inputs: Vec<WheeledInput>,
    grade_limited: u32,
    recovered: u32,
}

/// Both ground models as a [`DetScenario`] (`ground_vehicle_mix`, ADR-8551): four worlds (gravity
/// 9.81, 1.62, 3.71 and 24.79 m/s²), each with 8 tracked vehicles (two parameter sets, one without
/// pivot turns) and 8 wheeled ones (two parameter sets) on [`GroundField`], 1800 ticks at 60 Hz.
/// Every vehicle draws new seeded inputs every 30 ticks (the same stream order on every target:
/// world, then tanks, then cars); a car pulls the handbrake on some draws, and every 7th draw of a
/// vehicle is released (coast). The golden hashes every state word of every vehicle and the counts
/// of grade-limited and recovered steps, so a target whose slope, trig or integration differs shows
/// up as a diverging tick.
#[derive(Debug, Clone)]
pub struct GroundVehicleMix {
    worlds: Vec<MixWorld>,
    rng: SplitMix64,
    dt: f32,
    draws: u32,
}

impl GroundVehicleMix {
    /// Vehicles of each model per world.
    pub const PER_MODEL: u32 = 8;
    /// Ticks between new input draws.
    pub const INPUT_PERIOD: u32 = 30;
    /// The four worlds' gravities, m/s².
    pub const GRAVITIES: [f32; 4] = [EARTH_G, 1.62, 3.71, 24.79];

    /// The two tracked parameter sets: the default, and a light, fast one without pivot turns and
    /// with a 25° grade limit.
    pub fn tracked_types() -> [TrackedParams; 2] {
        [
            TrackedParams::DEFAULT,
            TrackedParams {
                separation_m: 1.6,
                max_track_speed_m_s: 12.0,
                friction_accel_m_s2: 6.0,
                coast_drag_m_s2: 0.5,
                pivot_turn: false,
                grade_limit_deg: 25.0,
                grade_speed_loss: 0.8,
                half_length_m: 1.8,
            },
        ]
    }

    /// The two wheeled parameter sets: the default car, and a short, grippy buggy with a 20° grade
    /// limit.
    pub fn wheeled_types() -> [WheeledParams; 2] {
        [
            WheeledParams::DEFAULT,
            WheeledParams {
                wheelbase_m: 2.1,
                max_steer_rad: 0.7,
                steer_rate_rad_s: 4.0,
                max_speed_m_s: 25.0,
                reverse_speed_m_s: 6.0,
                accel_m_s2: 8.0,
                brake_m_s2: 14.0,
                coast_m_s2: 2.0,
                lateral_grip_m_s2: 11.0,
                grade_limit_deg: 20.0,
                track_width_m: 1.5,
            },
        ]
    }

    /// The scenario reset for its default run.
    pub fn new() -> Self {
        let mut s = GroundVehicleMix {
            worlds: Vec::new(),
            rng: SplitMix64::new(0),
            dt: TickRate::DEFAULT.dt(),
            draws: 0,
        };
        s.reset(&s.default_run());
        s
    }

    /// The tracked vehicles of world `w` (0..4), or an empty slice.
    pub fn tanks(&self, w: usize) -> &[TrackedState] {
        self.worlds.get(w).map_or(&[], |m| m.tanks.as_slice())
    }

    /// The wheeled vehicles of world `w` (0..4), or an empty slice.
    pub fn cars(&self, w: usize) -> &[WheeledState] {
        self.worlds.get(w).map_or(&[], |m| m.cars.as_slice())
    }

    /// Grade-limited steps so far, summed over every vehicle of every world.
    pub fn grade_limited_steps(&self) -> u32 {
        self.worlds.iter().map(|m| m.grade_limited).sum()
    }

    /// Recovered (poisoned-state) steps so far, summed over every world.
    pub fn recovered_steps(&self) -> u32 {
        self.worlds.iter().map(|m| m.recovered).sum()
    }

    fn count(m: &mut MixWorld, report: GroundReport) {
        m.grade_limited += u32::from(report.grade_limited);
        m.recovered += u32::from(report.recovered);
    }
}

impl Default for GroundVehicleMix {
    fn default() -> Self {
        GroundVehicleMix::new()
    }
}

impl DetScenario for GroundVehicleMix {
    fn name(&self) -> &str {
        "ground_vehicle_mix"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x6E0D_7EC1, 1800, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let n = Self::PER_MODEL as usize;
        self.worlds.clear();
        for (w, gravity) in Self::GRAVITIES.into_iter().enumerate() {
            let world = WorldPhysics {
                gravity_m_s2: gravity,
                air_density_kg_m3: 0.0,
                vacuum: w == 1,
                low_g: LowG::Governor,
            };
            let mut tanks = Vec::with_capacity(n);
            let mut cars = Vec::with_capacity(n);
            for i in 0..Self::PER_MODEL {
                let x = (i % 4) as f32 * 16.0 - 24.0 + w as f32 * 3.0;
                let z = (i / 4) as f32 * 20.0 - 30.0;
                let yaw = i as f32 * 0.71 - 2.0;
                tanks.push(TrackedState::at_rest([x, GroundField.floor(x, z), z], yaw));
                let cz = z + 40.0;
                cars.push(WheeledState::at_rest([x, GroundField.floor(x, cz), cz], -yaw));
            }
            self.worlds.push(MixWorld {
                world,
                tanks,
                cars,
                tank_inputs: vec![TrackedInput::default(); n],
                car_inputs: vec![WheeledInput::default(); n],
                grade_limited: 0,
                recovered: 0,
            });
        }
        self.rng = SplitMix64::new(run.seed);
        self.dt = run.rate.dt();
        self.draws = 0;
    }

    fn step(&mut self, tick: u32) {
        if tick.is_multiple_of(Self::INPUT_PERIOD) {
            let release = self.draws % 7 == 3;
            self.draws += 1;
            for m in &mut self.worlds {
                for input in &mut m.tank_inputs {
                    let left = self.rng.signed() * 0.4 + 0.6 * self.rng.unit();
                    let right = self.rng.signed() * 0.4 + 0.6 * self.rng.unit();
                    let pivot = self.rng.unit() < 0.15;
                    *input = if release {
                        TrackedInput::default()
                    } else if pivot {
                        TrackedInput {
                            left_throttle: 1.0,
                            right_throttle: -1.0,
                        }
                    } else {
                        TrackedInput {
                            left_throttle: left,
                            right_throttle: right,
                        }
                    };
                }
                for input in &mut m.car_inputs {
                    let throttle = self.rng.unit() * 1.4 - 0.3;
                    let steer = self.rng.signed();
                    let roll = self.rng.unit();
                    *input = if release {
                        WheeledInput {
                            steer,
                            ..WheeledInput::default()
                        }
                    } else {
                        WheeledInput {
                            throttle,
                            brake: if roll < 0.1 { 0.7 } else { 0.0 },
                            steer,
                            handbrake: roll > 0.92,
                        }
                    };
                }
            }
        }
        let tank_types = Self::tracked_types();
        let car_types = Self::wheeled_types();
        let dt = self.dt;
        for m in &mut self.worlds {
            let mut reports = [GroundReport::default(); 2 * Self::PER_MODEL as usize];
            let (tank_reports, car_reports) = reports.split_at_mut(Self::PER_MODEL as usize);
            for (i, ((s, input), out)) in m
                .tanks
                .iter_mut()
                .zip(&m.tank_inputs)
                .zip(tank_reports)
                .enumerate()
            {
                *out = tracked::step(&tank_types[i % 2], &m.world, &GroundField, s, input, dt);
            }
            for (i, ((s, input), out)) in m.cars.iter_mut().zip(&m.car_inputs).zip(car_reports).enumerate() {
                *out = wheeled::step(&car_types[i % 2], &m.world, &GroundField, s, input, dt);
            }
            for report in reports {
                Self::count(m, report);
            }
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        let mut tank = 0;
        let mut car = 0;
        for m in &self.worlds {
            for s in &m.tanks {
                h.item("tank", tank);
                tank += 1;
                h.f32("pos_x", s.pos[0]);
                h.f32("pos_y", s.pos[1]);
                h.f32("pos_z", s.pos[2]);
                h.f32("yaw", s.yaw);
                h.f32("left_speed", s.left_speed);
                h.f32("right_speed", s.right_speed);
                h.f32("pitch", s.pitch);
                h.f32("roll", s.roll);
            }
            for s in &m.cars {
                h.item("car", car);
                car += 1;
                h.f32("pos_x", s.pos[0]);
                h.f32("pos_y", s.pos[1]);
                h.f32("pos_z", s.pos[2]);
                h.f32("yaw", s.yaw);
                h.f32("speed", s.speed);
                h.f32("steer", s.steer);
                h.f32("slip", s.slip);
                h.f32("pitch", s.pitch);
                h.f32("roll", s.roll);
            }
        }
        h.u32("grade_limited", self.grade_limited_steps());
        h.u32("recovered", self.recovered_steps());
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.worlds.first_mut().and_then(|m| m.tanks.first_mut()) {
            Some(s) => {
                s.pos[0] = f32::from_bits(s.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}
