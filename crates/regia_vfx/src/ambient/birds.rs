// SPDX-License-Identifier: MPL-2.0
//! Birds (ADR-3024): a flock, or a single soaring bird, around an anchor.
//!
//! [`AmbientBirds`] on an entity with a transform is the anchor. [`BirdFlockSim`] (inserted next
//! to it) flies the birds on a fixed step: each bird steers by Reynolds' behaviours (separation,
//! alignment, cohesion; Reynolds 1987) plus a seek toward a target that runs along a closed
//! Catmull-Rom spline (Catmull and Rom 1974) through seeded points around the anchor. A soaring
//! bird follows a slow circle instead and mostly glides. Wings flap in bursts with glides between
//! (flap-gliding). The same seed and the same frame steps give the same flight: no trigonometry
//! from the platform's maths library in the step (the path's points use
//! [`regia_core::math::det_cos`]).
//!
//! [`FollowAmbientBird`] moves any entity (an audio call's emitter) to a bird every frame, so
//! a call is heard where the bird is seen; [`BirdFlockSim::nearest_visible`] picks the bird.

use regia_core::math::{Rng, Vec3, det_cos};
use regia_ecs::prelude::*;

use super::MAX_BIRDS;

/// The fixed step of the flight, seconds.
pub const BIRD_STEP_S: f32 = 1.0 / 60.0;
/// The most steps one frame runs (a long hitch drops the rest instead of spiralling).
const MAX_STEPS_PER_FRAME: u32 = 8;
/// Points of a flock's closed path.
const PATH_POINTS: usize = 6;

/// How a group of birds flies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BirdMode {
    /// A flock wandering a loop around the anchor, flapping in bursts.
    Flock,
    /// One large bird (or a few) circling high on a thermal, mostly gliding.
    Soaring,
}

impl BirdMode {
    /// The stable lowercase name the agent protocol uses (`flock`, `soaring`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Flock => "flock",
            Self::Soaring => "soaring",
        }
    }

    /// The mode called `name` (`flock`; `soaring` or `single`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "flock" => Some(Self::Flock),
            "soaring" | "single" => Some(Self::Soaring),
            _ => None,
        }
    }
}

/// Birds around this entity's position (opt-in; drawn while [`super::AmbientLifeSettings`] has
/// birds on). Changing it restarts the flight; moving the entity carries the flight along.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct AmbientBirds {
    /// Flock or soaring.
    pub mode: BirdMode,
    /// Birds, `1..=MAX_BIRDS`.
    pub count: u32,
    /// Radius of the path around the anchor, metres.
    pub radius_m: f32,
    /// Flying height over the anchor, metres.
    pub height_m: f32,
    /// Wingspan, metres.
    pub wingspan_m: f32,
    /// Cruise speed, m/s.
    pub speed_m_per_s: f32,
    /// Linear body color.
    pub color: [f32; 3],
    /// The sounds (by `SoundAssets` name) that are this group's calls: a one-shot audio emitter
    /// playing one of them, started within reach of the anchor, follows the nearest visible bird
    /// (the framework's bird-call binding).
    pub call_sounds: Vec<String>,
    /// Seed of the path and the flight.
    pub seed: u32,
}

impl AmbientBirds {
    /// A flock of 12 small birds (25 cm wingspan, 10 m/s) on a 30 m loop 18 m up.
    #[must_use]
    pub fn flock() -> Self {
        Self {
            mode: BirdMode::Flock,
            count: 12,
            radius_m: 30.0,
            height_m: 18.0,
            wingspan_m: 0.25,
            speed_m_per_s: 10.0,
            color: [0.035, 0.032, 0.03],
            call_sounds: Vec::new(),
            seed: 0,
        }
    }

    /// One large bird (1.2 m wingspan, 9 m/s) circling 45 m up on a 40 m circle.
    #[must_use]
    pub fn soaring() -> Self {
        Self {
            mode: BirdMode::Soaring,
            count: 1,
            radius_m: 40.0,
            height_m: 45.0,
            wingspan_m: 1.2,
            speed_m_per_s: 9.0,
            color: [0.06, 0.045, 0.035],
            call_sounds: Vec::new(),
            seed: 0,
        }
    }

    /// The preset of `mode`.
    #[must_use]
    pub fn preset(mode: BirdMode) -> Self {
        match mode {
            BirdMode::Flock => Self::flock(),
            BirdMode::Soaring => Self::soaring(),
        }
    }

    /// How far from the anchor a call may start and still be bound to a bird: the path radius
    /// plus its height plus 20 m.
    #[must_use]
    pub fn call_reach_m(&self) -> f32 {
        self.radius_m.max(0.0) + self.height_m.abs() + 20.0
    }
}

/// Keeps this entity's `Transform` on bird `bird` of the [`BirdFlockSim`] on entity `flock`
/// (an audio call's emitter, a light, anything). Removed when the flock goes away.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowAmbientBird {
    /// The entity carrying the [`AmbientBirds`] and its [`BirdFlockSim`].
    pub flock: Entity,
    /// The bird's index in the flock.
    pub bird: u32,
}

/// One bird as the draw sees it: 48 bytes in the frame's uniform.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BirdInstance {
    /// Eye-relative position (xyz, metres) and the wingspan (w, metres).
    pub position_size: [f32; 4],
    /// Unit heading (xyz) and the wing angle over the body plane (w, radians).
    pub heading_wing: [f32; 4],
    /// Linear color (rgb) and opacity (a).
    pub color_alpha: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bird {
    position: Vec3,
    previous: Vec3,
    velocity: Vec3,
    /// Flap phase, radians, wrapped to `0..TAU`.
    phase: f32,
    /// Wing angle now and one step ago (eased between flap and glide).
    wing: f32,
    previous_wing: f32,
    flapping: bool,
    /// Seconds left in this flap burst or glide.
    timer: f32,
    /// This bird's lead along the path (its own place in the flock), metres.
    lead: Vec3,
}

/// The flight of one [`AmbientBirds`]: fixed-step, seeded. Inserted next to it by
/// [`super::step_ambient_birds`].
#[derive(Component, Clone, Debug, PartialEq)]
pub struct BirdFlockSim {
    config: AmbientBirds,
    anchor: Vec3,
    path: [Vec3; PATH_POINTS],
    /// The target's place on the path, in path segments (`0..PATH_POINTS`).
    path_u: f32,
    birds: Vec<Bird>,
    rng: Rng,
    accumulator: f32,
    steps: u64,
    /// Birds drawn (a real count; the last one fades), set by the draw's step.
    visible: f32,
}

fn sin_cos(radians: f32) -> (f32, f32) {
    (det_cos(radians - std::f32::consts::FRAC_PI_2), det_cos(radians))
}

fn catmull_rom(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, t: f32) -> Vec3 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (p2 - p0) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3)
}

impl BirdFlockSim {
    /// The flight of `config` anchored at `anchor`. Allocates the birds once.
    #[must_use]
    pub fn new(anchor: Vec3, config: &AmbientBirds) -> Self {
        let mut config = config.clone();
        config.count = config.count.clamp(1, MAX_BIRDS as u32);
        config.radius_m = if config.radius_m.is_finite() {
            config.radius_m.clamp(2.0, 2_000.0)
        } else {
            30.0
        };
        config.height_m = if config.height_m.is_finite() {
            config.height_m.clamp(1.0, 2_000.0)
        } else {
            18.0
        };
        config.wingspan_m = if config.wingspan_m.is_finite() {
            config.wingspan_m.clamp(0.05, 4.0)
        } else {
            0.25
        };
        config.speed_m_per_s = if config.speed_m_per_s.is_finite() {
            config.speed_m_per_s.clamp(1.0, 40.0)
        } else {
            10.0
        };
        let mut rng = Rng::with_stream(u64::from(config.seed), 0x6269_7264);
        let mut path = [Vec3::ZERO; PATH_POINTS];
        for (i, point) in path.iter_mut().enumerate() {
            let angle = std::f32::consts::TAU * (i as f32 + rng.range(-0.25..0.25f32)) / PATH_POINTS as f32;
            let (s, c) = sin_cos(angle);
            let r = match config.mode {
                BirdMode::Flock => config.radius_m * rng.range(0.55..1.0f32),
                BirdMode::Soaring => config.radius_m,
            };
            let y = match config.mode {
                BirdMode::Flock => config.height_m * rng.range(0.75..1.25f32),
                BirdMode::Soaring => config.height_m * rng.range(0.92..1.08f32),
            };
            *point = anchor + Vec3::new(r * c, y, r * s);
        }
        let start = catmull_rom(path[PATH_POINTS - 1], path[0], path[1], path[2], 0.0);
        let ahead = (path[1] - path[0]).normalize_or(Vec3::X);
        let birds = (0..config.count)
            .map(|_| {
                let lead = Vec3::new(
                    rng.range(-1.0..1.0f32),
                    rng.range(-0.5..0.5f32),
                    rng.range(-1.0..1.0f32),
                ) * (1.0 + 0.6 * (config.count as f32).sqrt());
                let position = start + lead;
                let (flap_min, flap_max) = flap_burst_s(config.mode);
                Bird {
                    position,
                    previous: position,
                    velocity: ahead * config.speed_m_per_s,
                    phase: rng.range(0.0..std::f32::consts::TAU),
                    wing: 0.1,
                    previous_wing: 0.1,
                    flapping: true,
                    timer: rng.range(flap_min..flap_max),
                    lead,
                }
            })
            .collect();
        Self {
            config,
            anchor,
            path,
            path_u: 0.0,
            birds,
            rng,
            accumulator: 0.0,
            steps: 0,
            visible: 0.0,
        }
    }

    /// The configuration flying (clamped to its ranges).
    #[must_use]
    pub fn config(&self) -> &AmbientBirds {
        &self.config
    }

    /// The anchor.
    #[must_use]
    pub fn anchor(&self) -> Vec3 {
        self.anchor
    }

    /// Moves the flight with its anchor: the path and the birds keep their places around it.
    /// Allocates nothing.
    pub fn move_anchor(&mut self, anchor: Vec3) {
        let by = anchor - self.anchor;
        self.anchor = anchor;
        for point in &mut self.path {
            *point += by;
        }
        for bird in &mut self.birds {
            bird.position += by;
            bird.previous += by;
        }
    }

    /// Birds in the flight.
    #[must_use]
    pub fn len(&self) -> usize {
        self.birds.len()
    }

    /// `true` when the flight has no birds (never: at least one).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.birds.is_empty()
    }

    /// Birds drawn now (a real count: the last one fades).
    #[must_use]
    pub fn visible(&self) -> f32 {
        self.visible
    }

    /// Sets how many birds are drawn (clamped to the flight's birds).
    pub fn set_visible(&mut self, visible: f32) {
        self.visible = if visible.is_finite() {
            visible.clamp(0.0, self.birds.len() as f32)
        } else {
            0.0
        };
    }

    /// Fixed steps run so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// The target on the path at `u` segments.
    fn path_point(&self, u: f32) -> Vec3 {
        let n = PATH_POINTS;
        let u = u.rem_euclid(n as f32);
        let i = (u.floor() as usize).min(n - 1);
        let t = u - i as f32;
        let p = |k: usize| self.path[(i + k + n - 1) % n];
        catmull_rom(p(0), p(1), p(2), p(3), t)
    }

    /// Advances by `dt` seconds of game time on the fixed step. Allocates nothing.
    pub fn step(&mut self, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.accumulator = (self.accumulator + dt).min(BIRD_STEP_S * MAX_STEPS_PER_FRAME as f32);
        while self.accumulator >= BIRD_STEP_S {
            self.accumulator -= BIRD_STEP_S;
            self.fixed_step();
        }
    }

    fn fixed_step(&mut self) {
        let h = BIRD_STEP_S;
        let speed = self.config.speed_m_per_s;
        let mode = self.config.mode;
        // The target runs along the path at the cruise speed.
        let here = self.path_point(self.path_u);
        let next = self.path_point(self.path_u + 0.05);
        let segment = ((next - here).length() / 0.05).max(1.0);
        self.path_u = (self.path_u + speed * h / segment).rem_euclid(PATH_POINTS as f32);
        let target = self.path_point(self.path_u + 0.15);
        let floor_y = self.anchor.y + 0.4 * self.config.height_m;
        let count = self.birds.len();
        for i in 0..count {
            let me = self.birds[i];
            let mut separation = Vec3::ZERO;
            let mut alignment = Vec3::ZERO;
            let mut cohesion = Vec3::ZERO;
            let mut neighbours = 0.0f32;
            for (j, other) in self.birds.iter().enumerate() {
                if j == i {
                    continue;
                }
                let away = me.position - other.position;
                let d2 = away.length_squared();
                if d2 < 64.0 {
                    neighbours += 1.0;
                    alignment += other.velocity;
                    cohesion += other.position;
                    if d2 < 2.25 && d2 > 1.0e-8 {
                        separation += away / d2;
                    }
                }
            }
            let seek_to = target + me.lead;
            let want = (seek_to - me.position).normalize_or(Vec3::X) * speed;
            let mut accel = (want - me.velocity) * 1.6;
            if neighbours > 0.0 {
                accel += (alignment / neighbours - me.velocity) * 0.5;
                accel += (cohesion / neighbours - me.position) * 0.3;
            }
            accel += separation * 6.0;
            if me.position.y < floor_y {
                accel.y += (floor_y - me.position.y) * 2.0;
            }
            let max_accel = match mode {
                BirdMode::Flock => 14.0,
                BirdMode::Soaring => 4.0,
            };
            let a_len = accel.length();
            if a_len > max_accel {
                accel *= max_accel / a_len;
            }
            let mut velocity = me.velocity + accel * h;
            let v_len = velocity.length();
            let (lo, hi) = (0.6 * speed, 1.3 * speed);
            if v_len < lo {
                velocity = velocity.normalize_or(Vec3::X) * lo;
            } else if v_len > hi {
                velocity *= hi / v_len;
            }
            let bird = &mut self.birds[i];
            bird.previous = bird.position;
            bird.previous_wing = bird.wing;
            bird.velocity = velocity;
            bird.position += velocity * h;
            // Flap bursts and glides.
            bird.timer -= h;
            if bird.timer <= 0.0 {
                bird.flapping = !bird.flapping;
                let (lo, hi) = if bird.flapping {
                    flap_burst_s(mode)
                } else {
                    glide_s(mode)
                };
                bird.timer = self.rng.range(lo..hi);
            }
            let hz = match mode {
                // Large birds beat slower: 6 Hz for the preset's finch, 2.5 Hz for a crow.
                BirdMode::Flock => (1.5 / self.config.wingspan_m).clamp(2.5, 8.0),
                BirdMode::Soaring => 2.2,
            };
            let target_wing = if bird.flapping {
                bird.phase = (bird.phase + std::f32::consts::TAU * hz * h).rem_euclid(std::f32::consts::TAU);
                0.6 * sin_cos(bird.phase).0
            } else {
                0.08
            };
            bird.wing += (target_wing - bird.wing) * (h * 25.0).min(1.0);
        }
        self.steps += 1;
    }

    /// Bird `index`'s position now (between the last two steps by the leftover time), world space.
    #[must_use]
    pub fn position(&self, index: usize) -> Option<Vec3> {
        let bird = self.birds.get(index)?;
        let t = (self.accumulator / BIRD_STEP_S).clamp(0.0, 1.0);
        Some(bird.previous.lerp(bird.position, t))
    }

    /// Bird `index`'s velocity, m/s.
    #[must_use]
    pub fn velocity(&self, index: usize) -> Option<Vec3> {
        self.birds.get(index).map(|bird| bird.velocity)
    }

    /// Opacity of bird `index` when `visible` (a real count) birds of the flight are drawn: the
    /// last one fades.
    #[must_use]
    pub fn alpha(index: usize, visible: f32) -> f32 {
        (visible - index as f32).clamp(0.0, 1.0)
    }

    /// The visible bird nearest `point` when `visible` birds are drawn: `(index, position)`.
    #[must_use]
    pub fn nearest_visible(&self, point: Vec3, visible: f32) -> Option<(usize, Vec3)> {
        (0..self.birds.len())
            .filter(|&i| Self::alpha(i, visible) > 0.0)
            .filter_map(|i| self.position(i).map(|p| (i, p)))
            .min_by(|a, b| {
                a.1.distance_squared(point)
                    .total_cmp(&b.1.distance_squared(point))
            })
    }

    /// Writes the draw's instances for `visible` birds, relative to `eye`, into `out`; returns
    /// how many it wrote. Allocates nothing.
    pub fn write_instances(&self, eye: [f64; 3], visible: f32, out: &mut [BirdInstance]) -> usize {
        let t = (self.accumulator / BIRD_STEP_S).clamp(0.0, 1.0);
        let mut written = 0;
        for (i, bird) in self.birds.iter().enumerate() {
            let alpha = Self::alpha(i, visible);
            if alpha <= 0.0 {
                break;
            }
            let Some(slot) = out.get_mut(written) else {
                break;
            };
            let p = bird.previous.lerp(bird.position, t);
            let heading = bird.velocity.normalize_or(Vec3::X);
            let wing = bird.previous_wing + (bird.wing - bird.previous_wing) * t;
            let rel = [
                (f64::from(p.x) - eye[0]) as f32,
                (f64::from(p.y) - eye[1]) as f32,
                (f64::from(p.z) - eye[2]) as f32,
            ];
            let c = self.config.color;
            *slot = BirdInstance {
                position_size: [rel[0], rel[1], rel[2], self.config.wingspan_m],
                heading_wing: [heading.x, heading.y, heading.z, wing],
                color_alpha: [c[0], c[1], c[2], alpha],
            };
            written += 1;
        }
        written
    }
}

fn flap_burst_s(mode: BirdMode) -> (f32, f32) {
    match mode {
        BirdMode::Flock => (0.6, 1.6),
        BirdMode::Soaring => (0.6, 1.2),
    }
}

fn glide_s(mode: BirdMode) -> (f32, f32) {
    match mode {
        BirdMode::Flock => (0.2, 0.8),
        BirdMode::Soaring => (3.0, 7.0),
    }
}
