// SPDX-License-Identifier: MPL-2.0
//! Feel numbers of a hull, measured by fixed-step runs of [`tick_hull`] (`detsim.hover_probe`).
//!
//! Each script is a scripted drive on a known floor, stepped at a fixed `dt` with no wall clock, so
//! two calls with the same hull, world and `dt` give the same numbers bit for bit
//! (`hover_probe_numbers_are_stable`). Speeds are horizontal, along the hull's facing
//! (`velocity_x * sin(yaw) + velocity_z * cos(yaw)`).
//!
//! | Script | Floor | Drive | Numbers |
//! |---|---|---|---|
//! | `accel` | flat at 0 | full throttle from rest for 30 s | `top_speed_m_s` (the highest forward speed), `time_to_90_s` (first time at 90 % of it) |
//! | `brake` | flat at 0 | `accel`, then throttle -1 until the forward speed is 0 or less (at most 30 s) | `stop_distance_m`, `stop_time_s` |
//! | `turn` | flat at 0 | throttle 1 while under half the top speed, else 0; straight to that speed, then full steer for 6 s | `turn_radius_m` (mean speed / mean yaw rate over the last 2 s), `turn_speed_m_s` |
//! | `hop` | flat at 0 | at rest, one hop on the first tick | `hop_apex_m` (above the ride height), `hop_air_s` (time airborne) |
//! | `crest` | 0 up to z = 0, a 1:5 rise to a 6 m peak at z = 30, a 1:5 fall back to 0 at z = 60 | full throttle from rest at z = -60 for 30 s | `crest_air_s` (time in the air after the peak), `crest_peak_m` (highest altitude above the floor then) |
//!
//! A number the run never reached (a hull that never stops braking within the cap, never leaves
//! its cushion over the crest) is reported as `None`.

use regia_detsim::float::det_max;
use regia_detsim::hover::host::{FlatFloor, FloorFn, FloorSampler, HoverBody};
use regia_detsim::hover::{HoverState, WorldPhysics};
use regia_detsim::math;

use super::{DetHoverHull, DetHoverInput, hop_speed, tick_hull};

/// A probe drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeScript {
    /// Full throttle from rest (`"accel"`).
    Accel,
    /// Full brake from top speed (`"brake"`).
    Brake,
    /// Full steer at half the top speed (`"turn"`).
    Turn,
    /// One hop from rest (`"hop"`).
    Hop,
    /// Full throttle over a crest (`"crest"`).
    Crest,
}

impl ProbeScript {
    /// Every script, in the order above.
    pub const ALL: [ProbeScript; 5] = [
        ProbeScript::Accel,
        ProbeScript::Brake,
        ProbeScript::Turn,
        ProbeScript::Hop,
        ProbeScript::Crest,
    ];

    /// The script called `name`; `None` for anything else.
    pub fn from_name(name: &str) -> Option<ProbeScript> {
        ProbeScript::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The script's name, as [`ProbeScript::from_name`] reads it.
    pub const fn name(self) -> &'static str {
        match self {
            ProbeScript::Accel => "accel",
            ProbeScript::Brake => "brake",
            ProbeScript::Turn => "turn",
            ProbeScript::Hop => "hop",
            ProbeScript::Crest => "crest",
        }
    }
}

/// What a probe measured. Only the script's own numbers are set.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ProbeNumbers {
    /// Highest forward speed under full throttle, m/s (`accel`, and the base of `brake` and `turn`).
    pub top_speed_m_s: Option<f32>,
    /// Seconds from rest to 90 % of the top speed (`accel`).
    pub time_to_90_s: Option<f32>,
    /// Metres from the start of the brake to a stop (`brake`).
    pub stop_distance_m: Option<f32>,
    /// Seconds from the start of the brake to a stop (`brake`).
    pub stop_time_s: Option<f32>,
    /// Turn radius at half the top speed, metres (`turn`).
    pub turn_radius_m: Option<f32>,
    /// The mean speed of the measured turn, m/s (`turn`).
    pub turn_speed_m_s: Option<f32>,
    /// Hop apex above the ride height, metres (`hop`).
    pub hop_apex_m: Option<f32>,
    /// Seconds airborne after the hop (`hop`).
    pub hop_air_s: Option<f32>,
    /// Seconds in the air after the crest's peak (`crest`).
    pub crest_air_s: Option<f32>,
    /// Highest altitude above the floor after the crest's peak, metres (`crest`).
    pub crest_peak_m: Option<f32>,
    /// Fixed ticks the probe stepped.
    pub ticks: u32,
}

/// The forward speed of a state, m/s.
pub fn forward_speed(state: &HoverState) -> f32 {
    state.velocity[0] * math::sin(state.yaw) + state.velocity[2] * math::cos(state.yaw)
}

/// Seconds of a probe's full-throttle and brake runs.
const RUN_S: f32 = 30.0;
/// Seconds of full steer in `turn`.
const TURN_S: f32 = 6.0;
/// Seconds the `turn` radius is averaged over (the end of the turn).
const TURN_MEASURE_S: f32 = 2.0;
/// Most seconds of the hop's flight.
const HOP_MAX_S: f32 = 10.0;

fn ticks_for(seconds: f32, dt: f32) -> u32 {
    // dt is checked by the caller (finite, above 0); the count is small for any dt >= 1e-4.
    let n = seconds / dt;
    if n.is_finite() && n > 0.0 {
        (n as u32).max(1)
    } else {
        1
    }
}

/// The crest floor of the `crest` script.
fn crest_floor(_x: f32, z: f32) -> f32 {
    if z <= 0.0 || z >= 60.0 {
        0.0
    } else if z <= 30.0 {
        z * 0.2
    } else {
        (60.0 - z) * 0.2
    }
}

struct Run<'a, S: FloorSampler + ?Sized> {
    hull: &'a DetHoverHull,
    world: &'a WorldPhysics,
    floor: &'a S,
    dt: f32,
    body: HoverBody,
    ticks: u32,
}

impl<S: FloorSampler + ?Sized> Run<'_, S> {
    fn tick(&mut self, input: DetHoverInput, hop: f32) {
        tick_hull(
            self.hull,
            &mut self.body,
            &input,
            self.floor,
            self.world,
            hop,
            self.dt,
        );
        self.ticks += 1;
    }
}

/// Run full throttle from rest for [`RUN_S`]: `(top speed, time to 90 %)`, leaving the run at speed.
fn accelerate<S: FloorSampler + ?Sized>(run: &mut Run<'_, S>) -> (f32, Option<f32>) {
    let n = ticks_for(RUN_S, run.dt);
    let mut speeds = Vec::with_capacity(n as usize);
    let mut top = 0.0_f32;
    for _ in 0..n {
        run.tick(DetHoverInput::from_axes([1.0, 0.0, 0.0]), 0.0);
        let v = forward_speed(&run.body.state);
        speeds.push(v);
        top = det_max(top, v);
    }
    let t90 = speeds
        .iter()
        .position(|&v| v >= 0.9 * top)
        .filter(|_| top > 0.0)
        .map(|i| (i as f32 + 1.0) * run.dt);
    (top, t90)
}

/// Measure `script` for `hull` on `world` at `dt`. A `dt` that is not finite and in `(0, 0.1]`
/// measures nothing (every number `None`).
pub fn probe(hull: &DetHoverHull, world: &WorldPhysics, dt: f32, script: ProbeScript) -> ProbeNumbers {
    let mut out = ProbeNumbers::default();
    if !(dt.is_finite() && dt > 0.0 && dt <= 0.1) {
        return out;
    }
    let flat = FlatFloor(0.0);
    let rest = HoverBody::at_rest([0.0, 0.0, 0.0], 0.0, &hull.params);
    match script {
        ProbeScript::Accel => {
            let mut run = Run {
                hull,
                world,
                floor: &flat,
                dt,
                body: rest,
                ticks: 0,
            };
            let (top, t90) = accelerate(&mut run);
            out.top_speed_m_s = Some(top);
            out.time_to_90_s = t90;
            out.ticks = run.ticks;
        }
        ProbeScript::Brake => {
            let mut run = Run {
                hull,
                world,
                floor: &flat,
                dt,
                body: rest,
                ticks: 0,
            };
            let (top, _) = accelerate(&mut run);
            out.top_speed_m_s = Some(top);
            let start = run.body.pos;
            let (s, c) = (math::sin(run.body.state.yaw), math::cos(run.body.state.yaw));
            for i in 0..ticks_for(RUN_S, dt) {
                run.tick(DetHoverInput::from_axes([-1.0, 0.0, 0.0]), 0.0);
                if forward_speed(&run.body.state) <= 0.0 {
                    let d = (run.body.pos[0] - start[0]) * s + (run.body.pos[2] - start[2]) * c;
                    out.stop_distance_m = Some(d);
                    out.stop_time_s = Some((i as f32 + 1.0) * dt);
                    break;
                }
            }
            out.ticks = run.ticks;
        }
        ProbeScript::Turn => {
            let mut top_run = Run {
                hull,
                world,
                floor: &flat,
                dt,
                body: rest,
                ticks: 0,
            };
            let (top, _) = accelerate(&mut top_run);
            out.top_speed_m_s = Some(top);
            let target = 0.5 * top;
            let mut run = Run {
                hull,
                world,
                floor: &flat,
                dt,
                body: rest,
                ticks: top_run.ticks,
            };
            let throttle = |body: &HoverBody| {
                if forward_speed(&body.state) < target {
                    1.0
                } else {
                    0.0
                }
            };
            for _ in 0..ticks_for(RUN_S, dt) {
                if forward_speed(&run.body.state) >= target {
                    break;
                }
                run.tick(DetHoverInput::from_axes([1.0, 0.0, 0.0]), 0.0);
            }
            let n = ticks_for(TURN_S, dt);
            let measure = ticks_for(TURN_MEASURE_S, dt).min(n);
            let (mut speed_sum, mut rate_sum) = (0.0_f32, 0.0_f32);
            for i in 0..n {
                let t = throttle(&run.body);
                run.tick(DetHoverInput::from_axes([t, 0.0, 1.0]), 0.0);
                if i >= n - measure {
                    let v = run.body.state.velocity;
                    speed_sum += math::sqrt(v[0] * v[0] + v[2] * v[2]);
                    rate_sum += run.body.state.yaw_rate.abs();
                }
            }
            let (speed, rate) = (speed_sum / measure as f32, rate_sum / measure as f32);
            out.turn_speed_m_s = Some(speed);
            out.turn_radius_m = (rate > 0.0).then(|| speed / rate);
            out.ticks = run.ticks;
        }
        ProbeScript::Hop => {
            let launch = hop_speed(hull, world, dt);
            let mut run = Run {
                hull,
                world,
                floor: &flat,
                dt,
                body: rest,
                ticks: 0,
            };
            let ride = hull.params.ride_height;
            let mut apex = run.body.state.altitude;
            let mut air = 0_u32;
            let mut was_airborne = false;
            for i in 0..ticks_for(HOP_MAX_S, dt) {
                let input = DetHoverInput {
                    hop: i == 0,
                    ..DetHoverInput::default()
                };
                run.tick(input, launch);
                apex = det_max(apex, run.body.state.altitude);
                if run.body.state.airborne {
                    air += 1;
                    was_airborne = true;
                } else if was_airborne {
                    break;
                }
            }
            if was_airborne {
                out.hop_apex_m = Some(apex - ride);
                out.hop_air_s = Some(air as f32 * dt);
            }
            out.ticks = run.ticks;
        }
        ProbeScript::Crest => {
            let floor = FloorFn(crest_floor);
            let start = HoverBody::at_rest([0.0, 0.0, -60.0], 0.0, &hull.params);
            let mut run = Run {
                hull,
                world,
                floor: &floor,
                dt,
                body: start,
                ticks: 0,
            };
            let mut air = 0_u32;
            let mut peak = 0.0_f32;
            let mut flew = false;
            for _ in 0..ticks_for(RUN_S, dt) {
                run.tick(DetHoverInput::from_axes([1.0, 0.0, 0.0]), 0.0);
                if run.body.pos[2] < 30.0 {
                    continue;
                }
                if hull.limits.hull_in_air(&hull.params, &run.body.state) {
                    air += 1;
                    flew = true;
                    peak = det_max(peak, run.body.state.altitude);
                } else if flew {
                    break;
                }
            }
            if flew {
                out.crest_air_s = Some(air as f32 * dt);
                out.crest_peak_m = Some(peak);
            }
            out.ticks = run.ticks;
        }
    }
    out
}
