// SPDX-License-Identifier: MPL-2.0
//! The bird's behaviour as a function of time (ADR-1502).
//!
//! - [`find_trigger`] reads the camera path and finds when the bird startles. That is the first
//!   path sample where the camera is within `distance_m`, or within `alert_distance_m` and
//!   closing at `closing_speed_m_s` or more, or where the wake's front reaches the bird with a
//!   peak gust of `gust_m_s` or more. Distance and closing speed are seen; the front is heard
//!   (and felt), and it is the only cue from a body above the speed of sound. The crossing is
//!   refined by bisection, so the time does not depend on the scan step.
//! - Before the trigger the bird idles. Its head turns and its feathers ruffle on a seeded
//!   schedule, in closed form in time.
//! - After `reaction_s` it flinches for `flinch_s`: it crouches, snaps its head toward the
//!   camera, flares its wings and widens its eyes.
//! - Then it takes off and flees. [`BirdSim`] integrates a point-mass flight at a fixed
//!   1/240 s step from the take-off. The bird steers away from where the camera was, climbs,
//!   jinks on a seeded schedule, and is thrown by the camera's wake (its gust over a 0.2 s drag
//!   response). After `glide_after_s` it stops flapping and glides away.
//!
//! The state at time `t` depends only on `t`, the spec and the path. Steps are fixed, and the
//! cached state is replayed from the take-off when time goes backwards. A render-out, a seek and
//! sub-frames all see the same bird.

use regia_core::math::wake::{WakeShape, smoothstep};
use regia_core::math::{EulerRot, Quat, Vec3};
use serde::Serialize;

use super::spec::{BirdBody, BirdSpec, Perch};
use crate::camera_body::PathSampler;
use crate::camera_body::proximity::closing_speed;

/// Fixed flight step, seconds.
pub const FLIGHT_STEP_S: f64 = 1.0 / 240.0;
/// Scan step of [`find_trigger`], seconds.
pub const TRIGGER_SCAN_S: f64 = 1.0 / 240.0;
/// Most samples one trigger scan takes, the span's two ends included: a path longer than
/// `MAX_TRIGGER_SAMPLES` times [`TRIGGER_SCAN_S`] (about 69 minutes) is scanned at a coarser
/// step over its whole span, so a long sequence (a timelapse) cannot stall the frame (as the
/// camera-body scanners cap theirs).
pub const MAX_TRIGGER_SAMPLES: u64 = 1_000_000;
/// Most flight steps one query integrates (10 minutes of flight).
const MAX_STEPS: u64 = 240 * 600;
/// Velocity response time of the flight, seconds.
const STEER_S: f32 = 0.35;
/// Drag response time to the camera's wake, seconds.
const WAKE_S: f32 = 0.2;
/// Gravity, m/s².
const G: f32 = 9.81;

/// What startled the bird.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerCause {
    /// The camera came within `distance_m`.
    Distance,
    /// The camera closed fast within `alert_distance_m`.
    Closing,
    /// The wake's front reached it.
    Wake,
}

/// When and why the bird startles.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Trigger {
    /// The cue's time on the camera path, seconds (the flinch starts `reaction_s` later).
    pub time_s: f64,
    /// What it was.
    pub cause: TriggerCause,
    /// Camera position at the cue.
    pub camera_position: [f32; 3],
    /// Camera velocity at the cue, m/s.
    pub camera_velocity: [f32; 3],
    /// Distance from the camera then, metres.
    pub distance_m: f32,
    /// Closing speed then, m/s.
    pub closing_m_s: f32,
}

/// Where the bird is in its sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BirdPhase {
    /// Perched, idling.
    Perched,
    /// The reflex delay after the cue: still, eyes widening.
    Alert,
    /// The snap pose before the take-off.
    Flinch,
    /// Panicked take-off.
    TakeOff,
    /// Erratic flapping flight away.
    Flee,
    /// Wings out, gliding away.
    Glide,
}

/// One flight step's state.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Flight {
    p: Vec3,
    v: Vec3,
    bank: f32,
    flap: f32,
    steps: u64,
}

/// The bird's pose at one instant: its root and the angles the mesh is posed with.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct BirdPose {
    /// Phase.
    pub phase: BirdPhase,
    /// Body centre, metres.
    pub position: Vec3,
    /// Body orientation (forward −Z, up +Y).
    pub rotation: Quat,
    /// Velocity, m/s.
    pub velocity: Vec3,
    /// Head yaw (left positive), radians.
    pub head_yaw: f32,
    /// Head pitch (up positive), radians.
    pub head_pitch: f32,
    /// Shoulder angle (wing raised positive), radians.
    pub wing_raise: f32,
    /// Outer wing angle relative to the inner (raised positive), radians.
    pub wing_tip: f32,
    /// Wing sweep (folded back positive), radians.
    pub wing_sweep: f32,
    /// Wing spread: 0 folded at the side, 1 fully open.
    pub wing_open: f32,
    /// Tail spread, `0..=1`.
    pub tail_spread: f32,
    /// Eye scale (1 calm, up to 1.6 startled).
    pub eye_scale: f32,
    /// Feather ruffle, `0..=1` (puffs the body out).
    pub ruffle: f32,
    /// Crouch, `0..=1` (the flinch lowers the body).
    pub crouch: f32,
    /// Legs out (1 standing, 0 tucked).
    pub legs: f32,
    /// Flap phase, radians.
    pub flap_phase: f32,
}

/// SplitMix64 finalizer (Steele, Lea and Flood 2014).
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A deterministic value in `-1..1` for `(seed, stream, index)`.
fn signed(seed: u64, stream: u64, index: i64) -> f32 {
    let h = mix(seed ^ mix(stream ^ mix(index as u64)));
    ((h >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

/// Where the camera is relative to the bird at `t`: `(distance, closing speed, body)`.
fn relation(
    path: &dyn PathSampler,
    t: f64,
    at: Vec3,
) -> Option<(f32, f32, regia_core::math::wake::WakeBody)> {
    let b = path.body(t, TRIGGER_SCAN_S)?;
    Some((
        b.position.distance(at),
        closing_speed(b.position, b.velocity, at),
        b,
    ))
}

/// The body centre of a bird perched with its feet at `feet`.
#[must_use]
pub fn perch_centre(spec: &BirdSpec) -> Vec3 {
    let body = spec.body();
    Vec3::from_array(spec.position) + Vec3::Y * (0.3 * body.length_m)
}

/// When the bird startles on `path` (see the module docs). `None` if it never does within the
/// path's span.
#[must_use]
pub fn find_trigger(spec: &BirdSpec, path: &dyn PathSampler, wake: &WakeShape) -> Option<Trigger> {
    let at = perch_centre(spec);
    let s = spec.startle;
    let cue = |t: f64| -> Option<TriggerCause> {
        let (d, closing, body) = relation(path, t, at)?;
        if d <= s.distance_m {
            return Some(TriggerCause::Distance);
        }
        if d <= s.alert_distance_m && closing >= s.closing_speed_m_s && s.closing_speed_m_s > 0.0 {
            return Some(TriggerCause::Closing);
        }
        let w = wake.sample(&body, at);
        if w.since_front_s >= 0.0 && w.since_front_s < 1.0 && w.peak_m_s >= s.gust_m_s && s.gust_m_s > 0.0 {
            return Some(TriggerCause::Wake);
        }
        None
    };
    let (start, end) = path.span();
    // An unbounded span (or a NaN end) has no end to scan to.
    if !start.is_finite() || !end.is_finite() || end < start {
        return None;
    }
    // At most `MAX_TRIGGER_SAMPLES` samples, the span's two ends included: a span that would
    // need more is scanned at a coarser step over its whole span, so a long sequence (a
    // timelapse) cannot stall the frame (as the camera-body scanners cap theirs). The grid is
    // `start + step * i` with `step` the span over the cap; when the cap binds, the span's end
    // takes the place of the grid point before it, so the cap holds with both ends sampled and
    // every other grid instant stays where it was (sprint-rev-w9-zai-v051-finish: a step of
    // span / (cap - 1) shifted the whole grid and missed a pass the scan used to find).
    let step = TRIGGER_SCAN_S.max((end - start) / MAX_TRIGGER_SAMPLES as f64);
    let n = (((end - start) / step).ceil() as u64).min(MAX_TRIGGER_SAMPLES - 1);
    let mut prev = start;
    for i in 0..=n {
        let t = if i == n { end } else { start + step * i as f64 };
        if let Some(cause) = cue(t) {
            // Bisect between the last quiet sample and this one.
            let (mut lo, mut hi) = (prev, t);
            if i > 0 {
                for _ in 0..30 {
                    let mid = 0.5 * (lo + hi);
                    if cue(mid).is_some() {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
            }
            let cause = cue(hi).unwrap_or(cause);
            let (d, closing, body) = relation(path, hi, at)?;
            return Some(Trigger {
                time_s: hi,
                cause,
                camera_position: body.position.to_array(),
                camera_velocity: body.velocity.to_array(),
                distance_m: d,
                closing_m_s: closing,
            });
        }
        prev = t;
    }
    None
}

/// Where to put a bird so the camera passes it at `time_s` with the given offsets (metres to
/// the camera's right, up and ahead along its travel), feet position.
#[must_use]
pub fn place_on_path(
    path: &dyn PathSampler,
    time_s: f64,
    right_m: f32,
    up_m: f32,
    ahead_m: f32,
) -> Option<[f32; 3]> {
    let b = path.body(time_s, TRIGGER_SCAN_S)?;
    let fwd = b.velocity.normalize_or(Vec3::NEG_Z);
    let right = fwd.cross(Vec3::Y).normalize_or(Vec3::X);
    let up = right.cross(fwd).normalize_or(Vec3::Y);
    Some((b.position + right * right_m + up * up_m + fwd * ahead_m).to_array())
}

/// Ground height at `(x, z)` for the flight's floor.
pub type FloorFn<'a> = &'a dyn Fn(f32, f32) -> Option<f32>;

/// What the flight reads from the world: the camera's wake (gust at a time and point) and the
/// ground.
pub struct FlightEnv<'a> {
    /// Gust of the camera's wake at `(time, point)`, m/s.
    pub gust: &'a dyn Fn(f64, Vec3) -> Vec3,
    /// Ground height, if known.
    pub floor: Option<FloorFn<'a>>,
}

/// One bird's behaviour over time (see the module docs).
#[derive(Clone, Debug)]
pub struct BirdSim {
    spec: BirdSpec,
    body: BirdBody,
    trigger: Option<Trigger>,
    cache: Option<Flight>,
}

impl BirdSim {
    /// A bird that startles at `trigger` (`None`: it never does).
    #[must_use]
    pub fn new(spec: BirdSpec, trigger: Option<Trigger>) -> Self {
        let body = spec.body();
        Self {
            spec,
            body,
            trigger,
            cache: None,
        }
    }

    /// The spec.
    #[must_use]
    pub fn spec(&self) -> &BirdSpec {
        &self.spec
    }

    /// The resolved size and colours.
    #[must_use]
    pub fn body(&self) -> &BirdBody {
        &self.body
    }

    /// The trigger.
    #[must_use]
    pub fn trigger(&self) -> Option<&Trigger> {
        self.trigger.as_ref()
    }

    /// Replaces the trigger (a new path); the flight is recomputed.
    pub fn set_trigger(&mut self, trigger: Option<Trigger>) {
        if self.trigger != trigger {
            self.trigger = trigger;
            self.cache = None;
        }
    }

    /// Time the flinch starts, seconds.
    #[must_use]
    pub fn flinch_time(&self) -> Option<f64> {
        self.trigger
            .map(|t| t.time_s + f64::from(self.spec.startle.reaction_s))
    }

    /// Time the take-off starts, seconds.
    #[must_use]
    pub fn takeoff_time(&self) -> Option<f64> {
        self.flinch_time()
            .map(|t| t + f64::from(self.spec.startle.flinch_s))
    }

    fn facing(&self) -> Quat {
        Quat::from_rotation_y(self.spec.facing_deg.to_radians())
    }

    /// Horizontal direction away from the camera at the cue (or backwards from the facing).
    fn flee_dir(&self) -> Vec3 {
        let at = perch_centre(&self.spec);
        let away = self.trigger.map_or(self.facing() * Vec3::Z, |t| {
            // Away from the camera's line of travel: off its axis, and forward with it a little
            // (birds break sideways from an approaching threat).
            let cam = Vec3::from_array(t.camera_position);
            let vel = Vec3::from_array(t.camera_velocity);
            let rel = at - cam;
            let dir = vel.normalize_or_zero();
            let side = rel - dir * rel.dot(dir);
            let side = Vec3::new(side.x, 0.0, side.z).normalize_or_zero();
            let fwd = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
            let raw = side + fwd * 0.35;
            if raw.length_squared() > 1.0e-6 {
                raw
            } else {
                Vec3::new(rel.x, 0.0, rel.z)
            }
        });
        Vec3::new(away.x, 0.0, away.z).normalize_or(self.facing() * Vec3::Z)
    }

    /// The jink offsets (yaw, pitch) at `tau` seconds after the take-off.
    fn jink(&self, tau: f32) -> (f32, f32) {
        let f = self.spec.flee;
        let k = (tau / f.jink_s).floor();
        let u = smoothstep(0.0, 0.35, tau / f.jink_s - k);
        let at = |i: f32, stream: u64| signed(self.spec.seed, stream, i as i64);
        let yaw = |i: f32| at(i, 11) * 50f32.to_radians() * f.erratic;
        let pitch = |i: f32| at(i, 12) * 15f32.to_radians() * f.erratic;
        let (y0, y1) = (if k > 0.0 { yaw(k - 1.0) } else { 0.0 }, yaw(k));
        let (p0, p1) = (if k > 0.0 { pitch(k - 1.0) } else { 0.0 }, pitch(k));
        (y0 + (y1 - y0) * u, p0 + (p1 - p0) * u)
    }

    fn launch(&self) -> Flight {
        let at = perch_centre(&self.spec);
        let away = self.flee_dir();
        let up = match self.spec.perch {
            Perch::Ground => 2.8,
            Perch::Branch | Perch::Post => 0.8,
        };
        Flight {
            p: at,
            v: away * 2.0 + Vec3::Y * up,
            bank: 0.0,
            flap: 0.0,
            steps: 0,
        }
    }

    fn step(&self, f: &mut Flight, t0: f64, env: &FlightEnv<'_>) {
        let dt = FLIGHT_STEP_S as f32;
        let tau = (f.steps as f64 * FLIGHT_STEP_S) as f32;
        let flee = self.spec.flee;
        let gliding = tau >= flee.glide_after_s;
        let takeoff = tau < flee.takeoff_s;
        let (jy, jp) = self.jink(tau);
        let climb = if gliding {
            -flee.glide_deg.to_radians()
        } else if takeoff {
            flee.takeoff_climb_deg.to_radians()
        } else {
            10f32.to_radians()
        };
        let speed = if gliding {
            flee.glide_speed_m_s
        } else if takeoff {
            flee.speed_m_s * (0.4 + 0.6 * tau / flee.takeoff_s)
        } else {
            flee.speed_m_s
        };
        let heading = Quat::from_rotation_y(jy) * self.flee_dir();
        let pitch = climb + jp;
        let want = (heading * pitch.cos() + Vec3::Y * pitch.sin()) * speed;
        let mut a = (want - f.v) / STEER_S;
        let t = t0 + f.steps as f64 * FLIGHT_STEP_S;
        let gust = (env.gust)(t, f.p);
        a += gust * (flee.wake_gain / WAKE_S);
        if let Some(floor) = env.floor
            && let Some(g) = floor(f.p.x, f.p.z)
        {
            let clear = g + 1.0;
            if f.p.y < clear {
                a.y += (clear - f.p.y) * 60.0 + (-f.v.y).max(0.0) * 10.0;
            }
        }
        // Bank into the turn: the lateral acceleration over gravity (a coordinated turn).
        let fwd = f.v.normalize_or(heading);
        let right = fwd.cross(Vec3::Y).normalize_or(Vec3::X);
        let lateral = a.dot(right);
        let target_bank = (lateral / G).atan().clamp(-1.1, 1.1);
        f.bank += (target_bank - f.bank) * (dt / 0.12).min(1.0);
        f.v += a * dt;
        f.p += f.v * dt;
        let hz = self.body.flap_hz * if takeoff { 1.25 } else { 1.0 };
        if !gliding {
            f.flap = (f.flap + std::f32::consts::TAU * hz * dt) % std::f32::consts::TAU;
        }
        f.steps += 1;
    }

    /// The flight state at `t` seconds (fixed steps from the take-off, cached).
    fn flight_at(&mut self, t: f64, env: &FlightEnv<'_>) -> Option<Flight> {
        let t0 = self.takeoff_time()?;
        if t < t0 {
            return None;
        }
        let target = (((t - t0) / FLIGHT_STEP_S).floor() as u64).min(MAX_STEPS);
        let mut f = match self.cache {
            Some(c) if c.steps <= target => c,
            _ => self.launch(),
        };
        while f.steps < target {
            self.step(&mut f, t0, env);
        }
        self.cache = Some(f);
        // Between two steps: carry on at the step's velocity (smooth sub-frames).
        let partial = (t - t0 - f.steps as f64 * FLIGHT_STEP_S) as f32;
        Some(Flight {
            p: f.p + f.v * partial,
            ..f
        })
    }

    /// The phase at `t`.
    #[must_use]
    pub fn phase_at(&self, t: f64) -> BirdPhase {
        let (Some(trigger), Some(flinch), Some(takeoff)) =
            (self.trigger, self.flinch_time(), self.takeoff_time())
        else {
            return BirdPhase::Perched;
        };
        let tau = t - takeoff;
        if t < trigger.time_s {
            BirdPhase::Perched
        } else if t < flinch {
            BirdPhase::Alert
        } else if t < takeoff {
            BirdPhase::Flinch
        } else if tau < f64::from(self.spec.flee.takeoff_s) {
            BirdPhase::TakeOff
        } else if tau < f64::from(self.spec.flee.glide_after_s) {
            BirdPhase::Flee
        } else {
            BirdPhase::Glide
        }
    }

    /// The idle head yaw at `t`: a new seeded target every 0.9 to 2.1 s, turned to in 0.12 s.
    fn idle_head(&self, t: f64) -> (f32, f32) {
        let slot = 1.5f64;
        let k = (t / slot).floor();
        let jitter = |i: f64| f64::from(signed(self.spec.seed, 21, i as i64)) * 0.4 * slot;
        let start = |i: f64| i * slot + jitter(i);
        let (k, begin) = if t < start(k) {
            (k - 1.0, start(k - 1.0))
        } else {
            (k, start(k))
        };
        let target = |i: f64, s: u64| signed(self.spec.seed, s, i as i64);
        let u = smoothstep(0.0, 0.12, (t - begin) as f32);
        let yaw = |i: f64| target(i, 22) * 70f32.to_radians();
        let pitch = |i: f64| target(i, 23) * 15f32.to_radians();
        (
            yaw(k - 1.0) + (yaw(k) - yaw(k - 1.0)) * u,
            pitch(k - 1.0) + (pitch(k) - pitch(k - 1.0)) * u,
        )
    }

    /// The idle feather ruffle at `t`: a 0.5 s shake roughly every 4 s.
    fn idle_ruffle(&self, t: f64) -> f32 {
        let period = 4.0f64;
        let k = (t / period).floor();
        let offset = (f64::from(signed(self.spec.seed, 31, k as i64)) * 0.5 + 0.5) * (period - 0.5);
        let local = (t - k * period - offset) as f32;
        if (0.0..0.5).contains(&local) {
            (local / 0.5 * std::f32::consts::PI).sin()
        } else {
            0.0
        }
    }

    /// The pose at `t` seconds of the camera path's clock (see the module docs).
    pub fn pose_at(&mut self, t: f64, env: &FlightEnv<'_>) -> BirdPose {
        let phase = self.phase_at(t);
        let centre = perch_centre(&self.spec);
        let facing = self.facing();
        let calm = BirdPose {
            phase,
            position: centre,
            rotation: facing * Quat::from_rotation_x(8f32.to_radians()),
            velocity: Vec3::ZERO,
            head_yaw: 0.0,
            head_pitch: 0.0,
            wing_raise: 0.0,
            wing_tip: 0.0,
            wing_sweep: 0.0,
            wing_open: 0.0,
            tail_spread: 0.0,
            eye_scale: 1.0,
            ruffle: 0.0,
            crouch: 0.0,
            legs: 1.0,
            flap_phase: 0.0,
        };
        match phase {
            BirdPhase::Perched => {
                let (head_yaw, head_pitch) = self.idle_head(t);
                let breathe = (std::f64::consts::TAU * 0.7 * t).sin() as f32;
                BirdPose {
                    head_yaw,
                    head_pitch,
                    ruffle: self.idle_ruffle(t).max(0.0) + 0.03 * breathe,
                    ..calm
                }
            }
            BirdPhase::Alert | BirdPhase::Flinch => {
                let trigger = self.trigger.map_or(t, |tr| tr.time_s);
                let flinch = self.flinch_time().unwrap_or(t);
                let into = ((t - flinch) / f64::from(self.spec.startle.flinch_s)).clamp(0.0, 1.0) as f32;
                // The head snaps toward the camera (in the body's frame).
                let (snap_yaw, snap_pitch) = self.trigger.map_or((0.0, 0.0), |tr| {
                    let rel = facing.inverse() * (Vec3::from_array(tr.camera_position) - centre);
                    let yaw = (-rel.x).atan2(-rel.z).clamp(-1.4, 1.4);
                    let pitch = rel
                        .y
                        .atan2(Vec3::new(rel.x, 0.0, rel.z).length())
                        .clamp(-0.6, 0.8);
                    (yaw, pitch)
                });
                let snap = smoothstep(0.0, 0.05, (t - trigger) as f32);
                let (idle_yaw, idle_pitch) = self.idle_head(trigger);
                let eye = 1.0 + 0.6 * smoothstep(0.0, 0.06, (t - trigger) as f32);
                if phase == BirdPhase::Alert {
                    return BirdPose {
                        head_yaw: idle_yaw + (snap_yaw - idle_yaw) * snap,
                        head_pitch: idle_pitch + (snap_pitch - idle_pitch) * snap,
                        eye_scale: eye,
                        ruffle: 0.6 * snap,
                        ..calm
                    };
                }
                // Crouch down fast (the first 40 %), then spring; wings flare open and up.
                let crouch = if into < 0.4 {
                    smoothstep(0.0, 0.4, into)
                } else {
                    1.0 - smoothstep(0.4, 1.0, into)
                };
                let flare = smoothstep(0.0, 0.7, into);
                BirdPose {
                    position: centre - Vec3::Y * (0.08 * self.body.length_m * crouch),
                    head_yaw: snap_yaw,
                    head_pitch: snap_pitch,
                    wing_raise: 70f32.to_radians() * flare,
                    wing_tip: 15f32.to_radians() * flare,
                    wing_sweep: -10f32.to_radians() * flare,
                    wing_open: flare,
                    tail_spread: flare,
                    eye_scale: 1.6,
                    ruffle: 1.0,
                    crouch,
                    ..calm
                }
            }
            BirdPhase::TakeOff | BirdPhase::Flee | BirdPhase::Glide => {
                let Some(f) = self.flight_at(t, env) else {
                    return calm;
                };
                let takeoff = self.takeoff_time().unwrap_or(t);
                let tau = (t - takeoff) as f32;
                let fwd = f.v.normalize_or(self.flee_dir());
                let look = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
                // Remove the arc's roll about the forward axis: keep +Y up, then bank.
                let (yaw, pitch, _) = look.to_euler(EulerRot::YXZ);
                let rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, -f.bank);
                let (amp, dihedral) = match phase {
                    BirdPhase::TakeOff => (75f32.to_radians(), 10f32.to_radians()),
                    BirdPhase::Flee => (55f32.to_radians(), 8f32.to_radians()),
                    _ => (0.0, 6f32.to_radians()),
                };
                // Glide: the wingbeat dies out over 0.3 s.
                let fade = if phase == BirdPhase::Glide {
                    1.0 - smoothstep(0.0, 0.3, tau - self.spec.flee.glide_after_s)
                } else {
                    1.0
                };
                let phi = f.flap;
                let beat = phi.sin() + 0.25 * (2.0 * phi).sin();
                let eye = 1.6 - 0.5 * smoothstep(1.0, 3.0, tau);
                BirdPose {
                    phase,
                    position: f.p,
                    rotation,
                    velocity: f.v,
                    head_yaw: 0.0,
                    head_pitch: -0.1,
                    wing_raise: dihedral + amp * fade * beat,
                    wing_tip: 0.45 * amp * fade * (phi - 0.6).sin(),
                    wing_sweep: 18f32.to_radians() * fade * phi.cos().max(0.0),
                    wing_open: 1.0,
                    tail_spread: if phase == BirdPhase::Glide { 0.6 } else { 0.9 },
                    eye_scale: eye,
                    ruffle: 0.0,
                    crouch: 0.0,
                    legs: 1.0 - smoothstep(0.0, 0.4, tau),
                    flap_phase: phi,
                }
            }
        }
    }
}
