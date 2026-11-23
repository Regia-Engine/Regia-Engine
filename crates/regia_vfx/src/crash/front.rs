// SPDX-License-Identifier: MPL-2.0
//! The blast front's one clock (ADR-1497): where the front is, when it reaches a point, and the
//! wind it drags behind it.
//!
//! Everything that marks the front reads the same three functions, so nothing in a crash can
//! disagree about where the front is:
//!
//! - [`blast_front_radius`]: the front is `SHOCK_FRONT_SPEED × t` from the crash point `t`
//!   seconds after the detonation (343 m/s, sound at 20 °C, beyond the near field).
//! - [`blast_arrival_seconds`] / [`blast_front_reached`] / [`blast_arrival_step`]: when it reaches
//!   a point `distance` metres away. [`super::CrashSequence`] sends `BlastArrived` (and the
//!   camera shake) with [`blast_front_reached`], the gust puffs fire with it, the dust wall's
//!   rings spawn on it, and `regia_framework`'s shock shake uses the same speed constant.
//! - [`CrashScale::blast_wind`] / [`blast_gust`]: the wind behind the front, for vegetation
//!   (grass bends, branches shed their snow) past the dust wall.
//!
//! The wind follows public blast physics (`docs/provenance.md`):
//!
//! - Peak side-on overpressure from the Kinney–Graham fit (Kinney and Graham, *Explosive Shocks in
//!   Air*, 2nd ed., 1985) at the Hopkinson–Cranz scaled distance `Z = R / W^(1/3)`, with a surface
//!   burst's ground reflection taken as a charge `SURFACE_BURST_FACTOR` times larger.
//! - Positive-phase duration from the same reference's fit.
//! - Peak particle (wind) speed behind a shock from the Rankine–Hugoniot relations for an ideal
//!   gas (`γ = 1.4`): `u = (c₀ p / γP₀) / √(1 + (γ+1) p / 2γP₀)`.
//! - The wind over time is the Friedlander waveform `u(τ) = u₀ (1 − τ/t₊) e^(−b τ/t₊)`, which
//!   swings slightly negative (the suction phase) after `t₊`.

use regia_app::prelude::*;
use regia_core::math::Vec3;

use super::{CrashScale, SHOCK_FRONT_SPEED};

/// Ambient pressure, Pa (sea level).
pub const AMBIENT_PRESSURE_PA: f32 = 101_325.0;
/// How many times larger a free-air charge a surface burst acts as: the ground reflects the
/// hemisphere of the blast that would have gone down (2 for a rigid surface, about 1.8 on soil
/// and snow, which absorb some of it).
pub const SURFACE_BURST_FACTOR: f32 = 1.8;
/// Ratio of specific heats of air.
const GAMMA: f32 = 1.4;
/// Friedlander decay coefficient of the far-field positive phase.
const FRIEDLANDER_B: f32 = 1.0;
/// How long after the front the wind is still evaluated, in positive phases (the suction phase
/// is spent by then).
const GUST_TAIL_PHASES: f32 = 4.0;
/// Clock slack when testing whether the front has reached a point, seconds: it absorbs the
/// rounding of a fixed step summed in `f64` (a 60 Hz step stored as `f32` is 5e-8 relative off)
/// and keeps an exact multiple (343 m at 60 Hz = 60 steps) on its step, as
/// [`blast_arrival_step`] does.
const ARRIVAL_SLACK_S: f64 = 1.0e-6;

/// Radius of the blast front `elapsed` seconds after the detonation, metres (0 before it or
/// for a non-finite time).
#[must_use]
pub fn blast_front_radius(elapsed: f32) -> f32 {
    if elapsed.is_finite() {
        elapsed.max(0.0) * SHOCK_FRONT_SPEED
    } else {
        0.0
    }
}

/// Seconds the front takes to run `distance` metres (0 for a non-positive or NaN distance).
#[must_use]
pub fn blast_arrival_seconds(distance: f32) -> f64 {
    if distance.is_nan() || distance <= 0.0 {
        return 0.0;
    }
    f64::from(distance) / f64::from(SHOCK_FRONT_SPEED)
}

/// Whether a front `elapsed` seconds old has reached a point `distance` metres from the crash.
/// This is the one test every front marker uses: the `BlastArrived` event and its shake, the
/// gust puffs, and the grass gust.
#[must_use]
pub fn blast_front_reached(elapsed: f64, distance: f32) -> bool {
    elapsed.is_finite() && elapsed + ARRIVAL_SLACK_S >= blast_arrival_seconds(distance)
}

/// The fixed step (counted from the detonation's step) on which the front reaches `distance`
/// at `fixed_hz`, rounded up so it never arrives early. The same rule as
/// `regia_framework::shock_shake::ShockShake::arrival_frame`, so a host that schedules its
/// camera shake there shakes on the frame [`super::CrashSequence`] reports the arrival. A
/// non-positive or non-finite rate gives 0.
#[must_use]
pub fn blast_arrival_step(distance: f32, fixed_hz: f32) -> u64 {
    if !(fixed_hz.is_finite() && fixed_hz > 0.0) {
        return 0;
    }
    let steps = blast_arrival_seconds(distance) * f64::from(fixed_hz);
    (steps - 1.0e-9).ceil().max(0.0) as u64
}

/// The air behind the blast front at one distance ([`CrashScale::blast_wind`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Reflect)]
pub struct BlastWind {
    /// Peak side-on overpressure, Pa.
    pub overpressure_pa: f32,
    /// Peak wind speed right behind the front, m/s (outward).
    pub peak_wind: f32,
    /// Positive-phase duration, seconds: the outward wind lasts this long.
    pub positive_phase: f32,
}

impl BlastWind {
    /// Wind speed `since` seconds after the front passed, m/s (outward; negative in the suction
    /// phase, 0 before the front and once the tail is spent): the Friedlander waveform.
    #[must_use]
    pub fn speed_at(&self, since: f32) -> f32 {
        let t_plus = self.positive_phase.max(1e-4);
        if !since.is_finite() || since < 0.0 || since > GUST_TAIL_PHASES * t_plus {
            return 0.0;
        }
        let x = since / t_plus;
        self.peak_wind * (1.0 - x) * (-FRIEDLANDER_B * x).exp()
    }
}

/// Kinney–Graham peak overpressure ratio `p / P₀` at scaled distance `z` (m/kg^(1/3)).
fn kinney_graham_overpressure(z: f32) -> f32 {
    let z = z.clamp(0.05, 1.0e4);
    let num = 808.0 * (1.0 + (z / 4.5).powi(2));
    let den = ((1.0 + (z / 0.048).powi(2)) * (1.0 + (z / 0.32).powi(2)) * (1.0 + (z / 1.35).powi(2))).sqrt();
    num / den
}

/// Kinney–Graham positive-phase duration per cube root of charge, ms/kg^(1/3), at scaled
/// distance `z`.
fn kinney_graham_duration(z: f32) -> f32 {
    // Past a few hundred m/kg^(1/3) the fit sits on its asymptote (about 4.1 ms/kg^(1/3)); the
    // clamp keeps `z^10` inside `f32`.
    let z = z.clamp(0.05, 400.0);
    let num = 980.0 * (1.0 + (z / 0.54).powi(10));
    let den = (1.0 + (z / 0.02).powi(3)) * (1.0 + (z / 0.74).powi(6)) * (1.0 + (z / 6.9).powi(2)).sqrt();
    num / den
}

impl CrashScale {
    /// The wind behind the blast front `distance` metres from a surface burst of this scale
    /// (see the module docs). At 250 t: about 60 m/s at 500 m and 8 m/s at shot 4's 3.3 km,
    /// for about a quarter of a second.
    #[must_use]
    pub fn blast_wind(self, distance: f32) -> BlastWind {
        let w_kg = self.tonnes() * 1_000.0 * SURFACE_BURST_FACTOR;
        let cbrt = w_kg.cbrt();
        let z = distance.max(0.0) / cbrt;
        let ratio = kinney_graham_overpressure(z);
        let p = ratio * AMBIENT_PRESSURE_PA;
        let peak_wind =
            (SHOCK_FRONT_SPEED * ratio / GAMMA) / (1.0 + (GAMMA + 1.0) * ratio / (2.0 * GAMMA)).sqrt();
        BlastWind {
            overpressure_pa: p,
            peak_wind,
            positive_phase: kinney_graham_duration(z) * cbrt * 1.0e-3,
        }
    }
}

/// The blast wind at `point`, `elapsed` seconds after a crash of `scale` at `origin` on ground of
/// normal `normal` detonated: outward along the ground from the crash point, zero ahead of the
/// front ([`blast_front_reached`]) and once its tail is spent. m/s.
#[must_use]
pub fn blast_gust(origin: Vec3, normal: Vec3, scale: CrashScale, elapsed: f64, point: Vec3) -> Vec3 {
    let n = normal.normalize_or(Vec3::Y);
    let rel = point - origin;
    let flat = rel - n * rel.dot(n);
    let distance = rel.length();
    if !blast_front_reached(elapsed, distance) {
        return Vec3::ZERO;
    }
    let since = (elapsed - blast_arrival_seconds(distance)).max(0.0) as f32;
    let speed = scale.blast_wind(distance).speed_at(since);
    flat.normalize_or_zero() * speed
}

/// One blast front running now ([`BlastGusts`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveFront {
    /// The crash entity.
    pub source: Entity,
    /// Crash point (world).
    pub origin: Vec3,
    /// Ground normal.
    pub normal: Vec3,
    /// The crash's scale.
    pub scale: CrashScale,
    /// Seconds since the detonation.
    pub elapsed: f64,
    /// Seconds since the detonation one frame before (for motion vectors of what the gust moves).
    pub prev_elapsed: f64,
}

impl ActiveFront {
    /// Front radius now, metres ([`blast_front_radius`]).
    #[must_use]
    pub fn radius(&self) -> f32 {
        blast_front_radius(self.elapsed as f32)
    }

    /// Front radius one frame before, metres.
    #[must_use]
    pub fn prev_radius(&self) -> f32 {
        blast_front_radius(self.prev_elapsed as f32)
    }

    /// The blast wind of this front at `point`, m/s ([`blast_gust`]).
    #[must_use]
    pub fn wind_at(&self, point: Vec3) -> Vec3 {
        blast_gust(self.origin, self.normal, self.scale, self.elapsed, point)
    }
}

/// Every blast front running this frame, from the [`super::CrashSequence`]s
/// ([`super::publish_blast_gusts`]): the wind field a vegetation system reads to bend grass and
/// shake branches as the front passes. A front stays listed until it is past
/// [`BlastGusts::RETIRE_RADIUS`].
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct BlastGusts {
    fronts: Vec<ActiveFront>,
}

impl BlastGusts {
    /// Radius past which a front is no longer listed, metres (20 km: the wind there is a breeze).
    pub const RETIRE_RADIUS: f32 = 20_000.0;

    /// The fronts running this frame.
    #[must_use]
    pub fn fronts(&self) -> &[ActiveFront] {
        &self.fronts
    }

    /// The summed blast wind at `point`, m/s.
    #[must_use]
    pub fn wind_at(&self, point: Vec3) -> Vec3 {
        self.fronts.iter().map(|f| f.wind_at(point)).sum()
    }

    /// The front whose wind is strongest at `point` now or about to be (the one a vegetation
    /// layer around `point` should follow), if any.
    #[must_use]
    pub fn strongest_near(&self, point: Vec3) -> Option<&ActiveFront> {
        self.fronts.iter().max_by(|a, b| {
            let wa = a.scale.blast_wind(a.origin.distance(point)).peak_wind;
            let wb = b.scale.blast_wind(b.origin.distance(point)).peak_wind;
            wa.total_cmp(&wb)
        })
    }

    /// Replaces the list (the publisher's job; tests and hosts with their own clock may call it).
    pub fn set_fronts(&mut self, fronts: Vec<ActiveFront>) {
        self.fronts = fronts;
    }
}

/// Where the gust puffs fire ([`super::CrashSequence::gust_sites`]) when a host has no tree list:
/// `count` points area-uniform in a disc of `radius` metres around `center` in the plane of
/// `normal`, put on the ground by `ground` (`None` drops a point). Deterministic in `seed`.
#[must_use]
pub fn crash_gust_sites(
    center: Vec3,
    normal: Vec3,
    radius: f32,
    count: usize,
    seed: u64,
    ground: &dyn Fn(Vec3) -> Option<Vec3>,
) -> Vec<Vec3> {
    let n = normal.normalize_or(Vec3::Y);
    let (t, b) = super::ground_frame(n);
    let mut state = seed ^ 0x6775_7374_7369_7465;
    let radius = if radius.is_finite() { radius.max(0.0) } else { 0.0 };
    (0..count)
        .filter_map(|_| {
            let r = radius * super::unit(&mut state).sqrt();
            let phi = super::unit(&mut state) * std::f32::consts::TAU;
            ground(center + t * (r * phi.cos()) + b * (r * phi.sin()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: CrashScale = CrashScale::SHOWCASE;

    #[test]
    fn the_clock_is_one_speed() {
        assert_eq!(blast_front_radius(1.0), SHOCK_FRONT_SPEED);
        assert_eq!(blast_front_radius(-1.0), 0.0);
        assert_eq!(blast_front_radius(f32::NAN), 0.0);
        assert!((blast_arrival_seconds(3308.9) - f64::from(3308.9f32) / 343.0).abs() < 1e-9);
        assert_eq!(blast_arrival_seconds(-5.0), 0.0);
        // An exact multiple stays on its step; anything past it rounds up.
        assert_eq!(blast_arrival_step(343.0, 60.0), 60);
        assert_eq!(blast_arrival_step(343.1, 60.0), 61);
        assert_eq!(blast_arrival_step(3308.9, 60.0), 579);
        assert_eq!(blast_arrival_step(100.0, 0.0), 0);
        // `blast_front_reached` on a summed f32 step agrees with the step rule.
        let dt = 1.0f32 / 60.0;
        for d in [150.0, 343.0, 500.0, 686.0, 1_000.0, 3_308.9, 3_430.0, 7_777.7] {
            let step = blast_arrival_step(d, 60.0);
            let mut elapsed = 0.0f64;
            let mut got = None;
            for n in 0..10_000u64 {
                if blast_front_reached(elapsed, d) {
                    got = Some(n);
                    break;
                }
                elapsed += f64::from(dt);
            }
            assert_eq!(got, Some(step), "{d} m");
        }
    }

    /// Public far-field numbers: tens of kPa and ~60 m/s of wind at 500 m from 250 t, a few kPa
    /// and a stiff breeze at 3.3 km, with a positive phase of a few tenths of a second.
    #[test]
    fn the_blast_wind_falls_with_distance() {
        let near = SHOW.blast_wind(500.0);
        let far = SHOW.blast_wind(3_308.9);
        assert!((15_000.0..=50_000.0).contains(&near.overpressure_pa), "{near:?}");
        assert!((35.0..=90.0).contains(&near.peak_wind), "{near:?}");
        assert!((1_000.0..=6_000.0).contains(&far.overpressure_pa), "{far:?}");
        assert!((3.0..=15.0).contains(&far.peak_wind), "{far:?}");
        assert!((0.1..=0.8).contains(&far.positive_phase), "{far:?}");
        let mut last = f32::INFINITY;
        for d in [200.0, 400.0, 800.0, 1_600.0, 3_200.0, 6_400.0] {
            let w = SHOW.blast_wind(d).peak_wind;
            assert!(w < last, "falls with distance at {d}: {w}");
            last = w;
        }
        // Friedlander: full at the front, zero at the end of the positive phase, a small
        // reverse flow after it, nothing once the tail is spent.
        assert_eq!(far.speed_at(-0.01), 0.0);
        assert!((far.speed_at(0.0) - far.peak_wind).abs() < 1e-4);
        assert!(far.speed_at(far.positive_phase).abs() < 1e-4);
        let suction = far.speed_at(1.5 * far.positive_phase);
        assert!(suction < 0.0 && suction > -0.2 * far.peak_wind, "{suction}");
        assert_eq!(far.speed_at(5.0 * far.positive_phase), 0.0);
    }

    #[test]
    fn the_gust_is_outward_and_only_behind_the_front() {
        let origin = Vec3::new(700.0, 105.7, 5.0);
        let point = origin + Vec3::new(-2_000.0, 30.0, -2_000.0);
        let d = origin.distance(point);
        let arrive = blast_arrival_seconds(d);
        assert_eq!(
            blast_gust(origin, Vec3::Y, SHOW, arrive - 0.02, point),
            Vec3::ZERO
        );
        let g = blast_gust(origin, Vec3::Y, SHOW, arrive + 0.01, point);
        assert!(g.length() > 3.0, "{g:?}");
        assert!(g.y.abs() < 1e-4, "along the ground");
        let outward = (point - origin).with_y(0.0).normalize();
        assert!(g.normalize().dot(outward) > 0.999);
        assert_eq!(
            blast_gust(origin, Vec3::Y, SHOW, arrive + 30.0, point),
            Vec3::ZERO
        );
    }

    #[test]
    fn gust_sites_are_deterministic_and_on_the_ground() {
        let flat = |p: Vec3| Some(p);
        let a = crash_gust_sites(Vec3::ZERO, Vec3::Y, 200.0, 40, 3, &flat);
        let b = crash_gust_sites(Vec3::ZERO, Vec3::Y, 200.0, 40, 3, &flat);
        assert_eq!(a, b);
        assert_eq!(a.len(), 40);
        assert!(a.iter().all(|p| p.length() <= 200.0 + 1e-3 && p.y.abs() < 1e-4));
        let half = crash_gust_sites(Vec3::ZERO, Vec3::Y, 200.0, 40, 3, &|p: Vec3| {
            (p.x > 0.0).then_some(p)
        });
        assert!(half.len() < 40 && half.iter().all(|p| p.x > 0.0));
    }
}
