// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/handling.rs, commit c9513623f), ADR-8012.
// Mark: adapt. The rig half (sample, fly, measure, Report, WORLDS), lines 603-930. Named changes:
// the caller passes `HoverParams` (ADR-8012 A-8 names) instead of a hull-class table; float min/max
// go through `regia_detsim::float`; trig and sqrt go through `regia_detsim::math`. Class tables,
// baseline-game numbers and game identifiers are not copied. The telemetry dump helpers are new
// (ADR-8393).

//! Feel-target measurement rig (plan row S-08, ADR-8393).
//!
//! Fly a hull over scripted ground, measure the feel targets, and dump the report. The caller
//! passes [`HoverParams`]. Cruise speed stays the authored cap; [`world_params`] scales thrust by
//! the world's gravity the way [`crate::site_physics::thrust_scale`] does.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::float::{det_max, det_min};
use crate::hover::{HoverParams, HoverState, apply_hull_hop, compute_lean, hop_launch_speed, step_world};
use crate::math;
use crate::site_physics::{EARTH_G, thrust_scale};

/// Fixed step of the rig, the lockstep tick (`1/60` s at the default rate).
pub const DT: f32 = crate::tick::tick_dt();

/// Boost multiplier used when [`HoverParams::turbo_scale`] is 0.
///
/// A positive `turbo_scale` is that hull's own multiplier. 0 selects this constant, so a hull that
/// has not set a boost still has one. The step itself does not read `turbo_scale`; [`boost_params`]
/// applies it to a copy.
pub const BOOST_SCALE: f32 = 1.375;

/// Grade of the slope window, degrees.
pub const SLOPE_DEG: f32 = 25.0;

/// Worlds the rig measures: name and surface gravity (m/s²).
pub const WORLDS: [(&str, f32); 6] = [
    ("earth", 9.81),
    ("mars", 3.71),
    ("ganymede", 1.428),
    ("europa", 1.315),
    ("titan", 1.35),
    ("iapetus", 0.223),
];

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// One telemetry sample from [`fly`].
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Time at the end of the tick, seconds.
    pub t: f32,
    /// Speed along the hull's facing, m/s.
    pub fwd: f32,
    /// Speed to the hull's right, m/s.
    pub lat: f32,
    /// Horizontal speed, m/s.
    pub speed: f32,
    /// Yaw rate, rad/s.
    pub yaw_rate: f32,
    /// Height above the ground sample, metres.
    pub altitude: f32,
    /// Ground height passed into the step, metres.
    pub terrain: f32,
    /// Path length since the start of this [`fly`], metres.
    pub dist: f32,
}

/// Samples from one [`fly`] call, in time order.
pub type Trace = Vec<Sample>;

/// Ground height under world `(x, z)`, metres.
pub type Ground<'a> = &'a dyn Fn(f32, f32) -> f32;

/// Feel targets for one hull on one world.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Authored forward cap after the world thrust scale (the cap itself is not scaled), m/s.
    pub cruise_m_s: f32,
    /// Forward speed at the end of the boost run, m/s.
    pub boost_m_s: f32,
    /// Time from rest to 90% of [`Report::cruise_m_s`], seconds. Infinite if the window ends first.
    pub t_to_cruise_s: f32,
    /// Time from rest to 90% of the boosted forward cap, seconds.
    pub t_to_boost_s: f32,
    /// Distance covered while coasting from cruise down through 0.3 m/s, metres.
    pub coast_dist_m: f32,
    /// Time to coast from cruise down through 0.3 m/s, seconds.
    pub coast_time_s: f32,
    /// Time to coast from cruise down to half of cruise, seconds.
    pub coast_half_speed_s: f32,
    /// Distance covered while braking from cruise down through 0.3 m/s, metres.
    pub brake_dist_m: f32,
    /// Time to brake from cruise down through 0.3 m/s, seconds.
    pub brake_time_s: f32,
    /// Yaw rate after 3 s of full steer in place, rad/s.
    pub spin_rate_rad_s: f32,
    /// Yaw rate at 1.2 s of a full-lock turn held at cruise, rad/s.
    pub turn_rate_cruise_rad_s: f32,
    /// Time for that turn to reach 90% of [`Report::turn_rate_cruise_rad_s`], seconds.
    pub turn_start_s: f32,
    /// Time for the yaw rate to fall to 10% of the cruise turn rate after steer is released, seconds.
    pub turn_stop_s: f32,
    /// Lateral speed after 5 s of full strafe from rest, m/s.
    pub strafe_speed_m_s: f32,
    /// Time to 90% of the strafe cap, seconds.
    pub strafe_t90_s: f32,
    /// Peak absolute visual pitch over 3 s of the cruise turn, degrees.
    pub lean_pitch_deg: f32,
    /// Peak absolute visual roll over that turn, degrees.
    pub lean_roll_turn_deg: f32,
    /// Peak absolute visual roll over 2 s of full strafe, degrees.
    pub lean_roll_strafe_deg: f32,
    /// Share of cruise speed kept after 1 s of full steer with the throttle released.
    pub slide_keep: f32,
    /// Time spent more than 0.25 m above the ride height over the crest, after 60 m of travel, seconds.
    pub crest_air_s: f32,
    /// Highest altitude after 60 m of travel over the crest, metres.
    pub crest_peak_m: f32,
    /// Time from passing 120 m until the hull is back within 0.1 m of its ride height, seconds.
    pub crest_settle_s: f32,
    /// Share of cruise speed kept after 3 s on the [`SLOPE_DEG`] ramp at full throttle.
    pub slope_speed_keep: f32,
    /// Uphill coast distance (down through 0.5 m/s) divided by the flat coast distance.
    pub uphill_coast_ratio: f32,
    /// Hop height above the altitude the hop left from, metres.
    pub hop_apex_m: f32,
}

/// Params as the step runs them on a world of gravity `g`: thrust scaled by [`thrust_scale`].
pub fn world_params(base: &HoverParams, g: f32) -> HoverParams {
    let mut params = *base;
    if let Some(scale) = thrust_scale(g) {
        params.thrust_accel *= scale;
    }
    params
}

/// [`world_params`], or the older thrust floor of 0.6 when `legacy_floor` is set.
///
/// The current floor ([`crate::site_physics::THRUST_FLOOR`]) keeps authored thrust on a world
/// lighter than Earth. The legacy floor still cuts thrust with gravity, and never below 0.6, and
/// it does not cap a world heavier than Earth.
pub fn world_params_for(base: &HoverParams, g: f32, legacy_floor: bool) -> HoverParams {
    if !legacy_floor {
        return world_params(base, g);
    }
    let mut params = *base;
    let ratio = g / EARTH_G;
    let floored = det_max(ratio, 0.6);
    let cap = if g < EARTH_G { 1.0 } else { f32::MAX };
    params.thrust_accel *= det_min(floored, cap);
    params
}

/// A copy of `base` with boost applied to the speed caps and the thrust.
pub fn boost_params(base: &HoverParams) -> HoverParams {
    let mut params = *base;
    let scale = if params.turbo_scale > 0.0 {
        params.turbo_scale
    } else {
        BOOST_SCALE
    };
    params.max_forward_speed *= scale;
    params.max_reverse_speed *= scale;
    params.max_strafe_speed *= scale;
    params.thrust_accel *= scale;
    params
}

/// Speed along the hull's facing, m/s.
pub fn fwd_of(state: &HoverState) -> f32 {
    state.velocity[0] * math::sin(state.yaw) + state.velocity[2] * math::cos(state.yaw)
}

/// Speed to the hull's right, m/s.
pub fn lat_of(state: &HoverState) -> f32 {
    state.velocity[0] * math::cos(state.yaw) - state.velocity[2] * math::sin(state.yaw)
}

/// A hull at its ride height, already moving forward at `fwd` (0 is at rest).
///
/// A hull that is already moving is treated as having got there under thrust, so releasing the
/// throttle starts the release drop.
pub fn hover_state_at(params: &HoverParams, fwd: f32) -> HoverState {
    HoverState {
        velocity: [0.0, 0.0, fwd],
        altitude: params.ride_height,
        thrusting: fwd != 0.0,
        ..HoverState::default()
    }
}

/// Flat ground at height 0.
pub fn flat(_x: f32, _z: f32) -> f32 {
    0.0
}

/// Fly `secs` over `ground`. Position integrates the same way a host does: the step, then the
/// velocity times [`DT`].
pub fn fly(
    params: &HoverParams,
    state: &mut HoverState,
    g: f32,
    secs: f32,
    input: &dyn Fn(f32) -> [f32; 3],
    ground: Ground<'_>,
) -> Trace {
    let n = (secs / DT).round() as usize;
    let mut pos = [0.0_f32, 0.0];
    let mut dist = 0.0_f32;
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..n {
        let t = i as f32 * DT;
        let terrain = ground(pos[0], pos[1]);
        step_world(params, state, &input(t), terrain, DT, g, 0.0);
        pos[0] += state.velocity[0] * DT;
        pos[1] += state.velocity[2] * DT;
        let speed = math::sqrt(state.velocity[0] * state.velocity[0] + state.velocity[2] * state.velocity[2]);
        dist += speed * DT;
        out.push(Sample {
            t: t + DT,
            fwd: fwd_of(state),
            lat: lat_of(state),
            speed,
            yaw_rate: state.yaw_rate,
            altitude: state.altitude,
            terrain,
            dist,
        });
    }
    out
}

/// Time of the first sample that matches `pred`, or infinity if none does.
pub fn time_to(trace: &Trace, pred: impl Fn(&Sample) -> bool) -> f32 {
    trace
        .iter()
        .find(|sample| pred(sample))
        .map_or(f32::INFINITY, |sample| sample.t)
}

/// Measure `params` on a world of gravity `g`, with the current thrust floor.
pub fn measure(params: &HoverParams, g: f32) -> Report {
    measure_with(params, g, false)
}

/// [`measure`] with an explicit thrust floor. `legacy_floor` selects the 0.6 floor.
///
/// One tracing span covers the whole measurement. The windows below are the copied ones: their
/// durations and thresholds are part of the rig, not a fit.
pub fn measure_with(base: &HoverParams, g: f32, legacy_floor: bool) -> Report {
    let span = tracing::debug_span!("handling_measure");
    let _enter = span.enter();
    let params = world_params_for(base, g, legacy_floor);
    let cruise = params.max_forward_speed;
    let t_to_cruise_s = accel_time(&params, g, cruise);
    let (boost_m_s, t_to_boost_s) = boost_run(base, g, legacy_floor);
    let coast = coast_run(&params, g, cruise);
    let brake = brake_run(&params, g, cruise);
    let spin_rate_rad_s = spin_rate(&params, g);
    let turn = turn_run(&params, g, cruise);
    let slide_keep = slide_keep(&params, g, cruise);
    let strafe = strafe_run(&params, g);
    let crest = crest_run(&params, g, cruise);
    let slope_speed_keep = slope_keep(&params, g, cruise);
    let uphill_coast_ratio = uphill_dist(&params, g, cruise) / coast.dist;
    let hop_apex_m = hop_apex(&params, g);
    Report {
        cruise_m_s: cruise,
        boost_m_s,
        t_to_cruise_s,
        t_to_boost_s,
        coast_dist_m: coast.dist,
        coast_time_s: coast.time,
        coast_half_speed_s: coast.half,
        brake_dist_m: brake.dist,
        brake_time_s: brake.time,
        spin_rate_rad_s,
        turn_rate_cruise_rad_s: turn.rate,
        turn_start_s: turn.start,
        turn_stop_s: turn.stop,
        strafe_speed_m_s: strafe.speed,
        strafe_t90_s: strafe.t90,
        lean_pitch_deg: turn.pitch_deg,
        lean_roll_turn_deg: turn.roll_deg,
        lean_roll_strafe_deg: strafe.roll_deg,
        slide_keep,
        crest_air_s: crest.air,
        crest_peak_m: crest.peak,
        crest_settle_s: crest.settle,
        slope_speed_keep,
        uphill_coast_ratio,
        hop_apex_m,
    }
}

/// One `name=value` line, in [`Report`] field order. The names are the dump contract.
pub fn format_report(report: &Report) -> String {
    let fields = [
        ("cruise_m_s", report.cruise_m_s),
        ("boost_m_s", report.boost_m_s),
        ("t_to_cruise_s", report.t_to_cruise_s),
        ("t_to_boost_s", report.t_to_boost_s),
        ("coast_dist_m", report.coast_dist_m),
        ("coast_time_s", report.coast_time_s),
        ("coast_half_speed_s", report.coast_half_speed_s),
        ("brake_dist_m", report.brake_dist_m),
        ("brake_time_s", report.brake_time_s),
        ("spin_rate_rad_s", report.spin_rate_rad_s),
        ("turn_rate_cruise_rad_s", report.turn_rate_cruise_rad_s),
        ("turn_start_s", report.turn_start_s),
        ("turn_stop_s", report.turn_stop_s),
        ("strafe_speed_m_s", report.strafe_speed_m_s),
        ("strafe_t90_s", report.strafe_t90_s),
        ("lean_pitch_deg", report.lean_pitch_deg),
        ("lean_roll_turn_deg", report.lean_roll_turn_deg),
        ("lean_roll_strafe_deg", report.lean_roll_strafe_deg),
        ("slide_keep", report.slide_keep),
        ("crest_air_s", report.crest_air_s),
        ("crest_peak_m", report.crest_peak_m),
        ("crest_settle_s", report.crest_settle_s),
        ("slope_speed_keep", report.slope_speed_keep),
        ("uphill_coast_ratio", report.uphill_coast_ratio),
        ("hop_apex_m", report.hop_apex_m),
    ];
    let mut out = String::new();
    for (name, value) in fields {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
        out.push('=');
        out.push_str(&value.to_string());
    }
    out
}

/// Write `text` to `path` by writing a sibling temporary file and renaming it into place.
///
/// The temporary name is unique to this process. A failed rename removes the temporary file.
/// `path` must name a file; a path with no file name returns [`io::ErrorKind::InvalidInput`].
pub fn write_report_atomic(path: &Path, text: &str) -> io::Result<()> {
    let parent = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let Some(file_name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "report path has no file name",
        ));
    };
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.{seq}.tmp", std::process::id()));
    let tmp = parent.join(tmp_name);
    fs::write(&tmp, text)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        }
    }
}

fn hold_forward(_t: f32) -> [f32; 3] {
    [1.0, 0.0, 0.0]
}

fn hold_still(_t: f32) -> [f32; 3] {
    [0.0; 3]
}

fn hold_brake(_t: f32) -> [f32; 3] {
    [-1.0, 0.0, 0.0]
}

fn hold_steer(_t: f32) -> [f32; 3] {
    [0.0, 0.0, 1.0]
}

fn hold_turn(_t: f32) -> [f32; 3] {
    [1.0, 0.0, 1.0]
}

fn hold_strafe(_t: f32) -> [f32; 3] {
    [0.0, 1.0, 0.0]
}

fn accel_time(params: &HoverParams, g: f32, cruise: f32) -> f32 {
    let mut state = hover_state_at(params, 0.0);
    let trace = fly(params, &mut state, g, 6.0, &hold_forward, &flat);
    time_to(&trace, |sample| sample.fwd >= 0.9 * cruise)
}

fn boost_run(base: &HoverParams, g: f32, legacy_floor: bool) -> (f32, f32) {
    let boosted = world_params_for(&boost_params(base), g, legacy_floor);
    let mut state = hover_state_at(&boosted, 0.0);
    let trace = fly(&boosted, &mut state, g, 8.0, &hold_forward, &flat);
    let speed = trace.last().map_or(0.0, |sample| sample.fwd);
    let t = time_to(&trace, |sample| sample.fwd >= 0.9 * boosted.max_forward_speed);
    (speed, t)
}

struct CoastFeel {
    dist: f32,
    time: f32,
    half: f32,
}

fn coast_run(params: &HoverParams, g: f32, cruise: f32) -> CoastFeel {
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 30.0, &hold_still, &flat);
    CoastFeel {
        time: time_to(&trace, |sample| sample.speed < 0.3),
        dist: trace
            .iter()
            .find(|sample| sample.speed < 0.3)
            .map_or(f32::NAN, |sample| sample.dist),
        half: time_to(&trace, |sample| sample.speed <= 0.5 * cruise),
    }
}

struct BrakeFeel {
    dist: f32,
    time: f32,
}

fn brake_run(params: &HoverParams, g: f32, cruise: f32) -> BrakeFeel {
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 10.0, &hold_brake, &flat);
    BrakeFeel {
        time: time_to(&trace, |sample| sample.speed < 0.3),
        dist: trace
            .iter()
            .find(|sample| sample.speed < 0.3)
            .map_or(f32::NAN, |sample| sample.dist),
    }
}

fn spin_rate(params: &HoverParams, g: f32) -> f32 {
    let mut state = hover_state_at(params, 0.0);
    let trace = fly(params, &mut state, g, 3.0, &hold_steer, &flat);
    trace.last().map_or(0.0, |sample| sample.yaw_rate)
}

struct TurnFeel {
    rate: f32,
    start: f32,
    stop: f32,
    pitch_deg: f32,
    roll_deg: f32,
}

fn turn_run(params: &HoverParams, g: f32, cruise: f32) -> TurnFeel {
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 4.0, &hold_turn, &flat);
    // Steady yaw is read at 1.2 s: long enough for the speed yaw lag to settle, throttle held.
    let rate = trace
        .iter()
        .find(|sample| sample.t >= 1.2)
        .map_or(0.0, |sample| sample.yaw_rate);
    let start = time_to(&trace, |sample| sample.yaw_rate >= 0.9 * rate);
    let (pitch, roll) = peak_lean(params, g, cruise, &hold_turn, 3.0);
    TurnFeel {
        rate,
        start,
        stop: turn_stop_time(params, g, cruise, rate),
        pitch_deg: pitch.to_degrees(),
        roll_deg: roll.to_degrees(),
    }
}

fn peak_lean(
    params: &HoverParams,
    g: f32,
    fwd: f32,
    input: &dyn Fn(f32) -> [f32; 3],
    secs: f32,
) -> (f32, f32) {
    let mut state = hover_state_at(params, fwd);
    let mut roll = 0.0_f32;
    let mut pitch = 0.0_f32;
    let n = (secs / DT) as usize;
    for _ in 0..n {
        let axes = input(0.0);
        step_world(params, &mut state, &axes, 0.0, DT, g, 0.0);
        let (pi, ro) = compute_lean(params, &state, &axes);
        roll = det_max(roll, ro.abs());
        pitch = det_max(pitch, pi.abs());
    }
    (pitch, roll)
}

/// Seconds for the yaw rate to fall below 10% of `rate` once steer is released.
fn turn_stop_time(params: &HoverParams, g: f32, cruise: f32, rate: f32) -> f32 {
    let mut state = hover_state_at(params, cruise);
    state.yaw_rate = rate;
    let trace = fly(params, &mut state, g, 3.0, &hold_still, &flat);
    time_to(&trace, |sample| sample.yaw_rate.abs() <= 0.1 * rate.abs())
}

fn slide_keep(params: &HoverParams, g: f32, cruise: f32) -> f32 {
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 1.0, &hold_steer, &flat);
    trace.last().map_or(0.0, |sample| sample.speed / cruise)
}

struct StrafeFeel {
    speed: f32,
    t90: f32,
    roll_deg: f32,
}

fn strafe_run(params: &HoverParams, g: f32) -> StrafeFeel {
    let mut state = hover_state_at(params, 0.0);
    let trace = fly(params, &mut state, g, 5.0, &hold_strafe, &flat);
    let speed = trace.last().map_or(0.0, |sample| sample.lat);
    let t90 = time_to(&trace, |sample| sample.lat >= 0.9 * params.max_strafe_speed);
    let (_pitch, roll) = peak_lean(params, g, 0.0, &hold_strafe, 2.0);
    StrafeFeel {
        speed,
        t90,
        roll_deg: roll.to_degrees(),
    }
}

struct CrestFeel {
    air: f32,
    peak: f32,
    settle: f32,
}

fn crest_run(params: &HoverParams, g: f32, cruise: f32) -> CrestFeel {
    let hump = |x: f32, z: f32| -> f32 {
        let d = math::sqrt(x * x + z * z) - 100.0;
        if d.abs() >= 20.0 {
            0.0
        } else {
            3.0 * (1.0 - d.abs() / 20.0)
        }
    };
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 8.0, &hold_forward, &hump);
    let high = trace
        .iter()
        .filter(|sample| sample.altitude > params.ride_height + 0.25 && sample.dist > 60.0)
        .count();
    let air = (high as f32) * DT;
    let mut peak = 0.0_f32;
    for sample in trace.iter().filter(|sample| sample.dist > 60.0) {
        peak = det_max(peak, sample.altitude);
    }
    let mut last_off = 0.0_f32;
    for sample in &trace {
        let off_ride = (sample.altitude - params.ride_height).abs() > 0.1;
        if sample.dist > 100.0 && off_ride {
            last_off = det_max(last_off, sample.t);
        }
    }
    let first_down = trace
        .iter()
        .find(|sample| sample.dist > 120.0)
        .map_or(0.0, |sample| sample.t);
    CrestFeel {
        air,
        peak,
        settle: det_max(last_off - first_down, 0.0),
    }
}

fn slope_ground(x: f32, z: f32) -> f32 {
    let slope = SLOPE_DEG.to_radians();
    let grade = math::sin(slope) / math::cos(slope);
    let d = math::sqrt(x * x + z * z);
    det_max(d, 0.0) * grade
}

fn slope_keep(params: &HoverParams, g: f32, cruise: f32) -> f32 {
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 3.0, &hold_forward, &slope_ground);
    trace.last().map_or(0.0, |sample| sample.speed / cruise)
}

fn uphill_dist(params: &HoverParams, g: f32, cruise: f32) -> f32 {
    let mut state = hover_state_at(params, cruise);
    let trace = fly(params, &mut state, g, 30.0, &hold_still, &slope_ground);
    trace
        .iter()
        .find(|sample| sample.speed < 0.5)
        .map_or(f32::NAN, |sample| sample.dist)
}

fn hop_apex(params: &HoverParams, g: f32) -> f32 {
    let mut state = hover_state_at(params, 0.0);
    let launch = hop_launch_speed(g, 0.0, DT);
    apply_hull_hop(&mut state, launch);
    let start = state.altitude;
    let mut apex = 0.0_f32;
    for _ in 0..20000 {
        step_world(params, &mut state, &[0.0; 3], 0.0, DT, g, 0.0);
        apex = det_max(apex, state.altitude - start);
        if !state.airborne {
            break;
        }
    }
    apex
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A low-g body's surface gravity, as test data (the value of the deprecated `site_physics::TITAN_G`;
    /// ADR-8349 keeps a body's gravity out of engine API).
    const TITAN_G: f32 = 1.35;

    #[test]
    fn legacy_floor_cuts_thrust_and_the_current_floor_does_not() {
        let base = HoverParams::DEFAULT_HULL;
        let current = world_params(&base, TITAN_G);
        let legacy = world_params_for(&base, TITAN_G, true);
        let scale = thrust_scale(TITAN_G).expect("titan g");
        assert_eq!(
            current.thrust_accel.to_bits(),
            (base.thrust_accel * scale).to_bits()
        );
        assert_eq!(legacy.thrust_accel.to_bits(), (base.thrust_accel * 0.6).to_bits());
        let earth = world_params_for(&base, EARTH_G, true);
        assert_eq!(earth.thrust_accel.to_bits(), base.thrust_accel.to_bits());
        let heavy = EARTH_G * 2.0;
        assert_eq!(
            world_params_for(&base, heavy, true).thrust_accel.to_bits(),
            world_params(&base, heavy).thrust_accel.to_bits()
        );
    }

    #[test]
    fn write_report_atomic_replaces_the_file_and_drops_the_temporary() {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-handling-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("report.txt");
        let text = "cruise_m_s=1\n";
        write_report_atomic(&path, text).expect("write");
        let got = fs::read_to_string(&path).expect("read");
        assert_eq!(got, text);
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).expect("list") {
            names.push(entry.expect("entry").file_name());
        }
        assert_eq!(names, vec![OsString::from("report.txt")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_with_no_file_name_is_an_input_error() {
        let error = write_report_atomic(Path::new(""), "x").expect_err("empty path");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
