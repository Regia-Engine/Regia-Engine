// SPDX-License-Identifier: MPL-2.0
//! Ambient life (ADR-3024): what makes a still outdoor scene feel alive. Motes (pollen and dust)
//! glinting in the sun, gnat and fly swarms, butterflies, fireflies at dusk, falling leaves by
//! season, and birds: flocks and single soaring birds whose positions the audio's bird calls can
//! follow, so a call is heard where a bird is seen.
//!
//! Opt-in: nothing runs or draws until [`AmbientLifeSettings::enabled`] is set (the agent's
//! `render.set_ambient_life`, or a host inserting the resource). Off on the Low tier and below
//! ([`AmbientBudget::for_quality`]).
//!
//! # How it is drawn
//!
//! Motes, insects and leaves are stateless instanced quads in boxes wrapped around the eye, the
//! same scheme as the rain ([`crate::precip`]): the CPU integrates one drift per box on game time
//! ([`AmbientIntegrator`]) and each vertex rebuilds its creature from its index and the seed. No
//! simulation buffer, no per-creature CPU work, one uniform write per frame, so a camera cut
//! shows a full field on its first frame and the same inputs draw the same bits. Every periodic
//! motion runs on a clock wrapped to [`AMBIENT_LOOP_S`] with frequencies that are whole cycles per
//! loop, so the wrap never shows.
//!
//! Birds are a small CPU flock ([`birds`]): Reynolds' steering behaviours (separation, alignment,
//! cohesion) toward a target that runs along a closed Catmull-Rom spline around the flock's anchor,
//! on a fixed step, seeded. Their positions are uploaded in the same uniform (at most
//! [`MAX_BIRDS`]).
//!
//! # Density
//!
//! [`AmbientActivity::of`] turns the time of day, season, rain, wind and cloud into a `0..1`
//! activity per kind; [`AmbientCounts::of`] scales the tier's budget by it. The CPU mirrors
//! ([`mote_at`], [`swarm_member_at`], [`butterfly_at`], [`firefly_at`], [`leaf_at`]) rebuild a
//! creature exactly as the vertex stage does, for the tests and the agent.

pub mod birds;
mod gpu;
pub(crate) use gpu::contract_rows;
mod system;

pub use birds::{AmbientBirds, BirdFlockSim, BirdInstance, BirdMode, FollowAmbientBird};
pub use gpu::{AmbientInstances, AmbientLifeGpu, AmbientView, ambient_shader_source};
pub use system::{
    AmbientLifeHandle, AmbientLifePlugin, AmbientLifeReport, AmbientState, AutoAmbientBirds, BIRD_PRESETS,
    ambient_tier, follow_ambient_birds, step_ambient_birds, step_ambient_life, unmatched_profile_tier,
};

use regia_app::prelude::*;
use regia_render::{QualityTier, TimeOfDay};

/// The animation clock wraps to this many seconds. Every periodic motion has a whole number of
/// cycles per loop, so the wrap is invisible and `f32` time keeps sub-millisecond precision.
pub const AMBIENT_LOOP_S: f64 = 256.0;
/// The most birds the draw carries (all flocks together).
pub const MAX_BIRDS: usize = 64;
/// Members of one swarm (gnats); a fly swarm uses the first [`FLY_SWARM_MEMBERS`].
pub const SWARM_MEMBERS: u32 = 40;
/// Members of a fly swarm.
pub const FLY_SWARM_MEMBERS: u32 = 4;
/// The part of each half-extent alpha fades over at a box edge.
const EDGE_FADE: f32 = 0.15;
/// The part of a drawn count alpha fades over.
const COUNT_FADE: f32 = 0.1;
/// Salts that give each kind its own hash stream.
const SALT_MOTE: u32 = 0x6d6f_7465;
const SALT_SWARM: u32 = 0x7377_726d;
const SALT_MEMBER: u32 = 0x6d65_6d62;
const SALT_BUTTERFLY: u32 = 0x6275_7466;
const SALT_FIREFLY: u32 = 0x6669_7265;
const SALT_LEAF: u32 = 0x6c65_6166;
const TAU: f32 = std::f32::consts::TAU;

/// The season, which picks what flies and what falls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Season {
    /// Pollen, the first insects, a few leaves.
    Spring,
    /// The most insects, butterflies and fireflies, a few green leaves.
    Summer,
    /// Falling leaves in reds, oranges and browns; fewer insects.
    Autumn,
    /// Dust only, a rare dry leaf, no insects.
    Winter,
}

impl Season {
    /// Every season, in order.
    pub const ALL: [Self; 4] = [Self::Spring, Self::Summer, Self::Autumn, Self::Winter];

    /// The stable lowercase name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spring => "spring",
            Self::Summer => "summer",
            Self::Autumn => "autumn",
            Self::Winter => "winter",
        }
    }

    /// The season called `name` (`spring`, `summer`, `autumn` or `fall`, `winter`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "spring" => Some(Self::Spring),
            "summer" => Some(Self::Summer),
            "autumn" | "fall" => Some(Self::Autumn),
            "winter" => Some(Self::Winter),
            _ => None,
        }
    }

    /// The astronomical season of `time`'s day and hemisphere: spring from day 80 (equinox),
    /// summer from 172, autumn from 266, winter from 355; shifted by half a year south of the
    /// equator.
    #[must_use]
    pub fn of_time(time: &TimeOfDay) -> Self {
        let day = u32::from(time.day_of_year.clamp(1, 365));
        let day = if time.latitude_deg < 0.0 {
            (day + 182 - 1) % 365 + 1
        } else {
            day
        };
        match day {
            80..=171 => Self::Spring,
            172..=265 => Self::Summer,
            266..=354 => Self::Autumn,
            _ => Self::Winter,
        }
    }

    /// The index of the season in [`Self::ALL`] (the shader's leaf palette row).
    #[must_use]
    pub fn index(self) -> u32 {
        match self {
            Self::Spring => 0,
            Self::Summer => 1,
            Self::Autumn => 2,
            Self::Winter => 3,
        }
    }
}

/// The project's ambient-life switches. Off by default.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct AmbientLifeSettings {
    /// Run and draw ambient life at all.
    pub enabled: bool,
    /// Pollen and dust motes in the sun.
    pub motes: bool,
    /// Gnat and fly swarms, butterflies and fireflies.
    pub insects: bool,
    /// Bird flocks and single birds.
    pub birds: bool,
    /// Falling leaves.
    pub leaves: bool,
    /// Multiplies every drawn count, `0..4`.
    pub density_scale: f32,
    /// The season; `None` follows the [`TimeOfDay`] (its day and hemisphere).
    pub season: Option<Season>,
    /// Seed of every placement and flight (same seed, same frames).
    pub seed: u32,
    /// Without any [`AmbientBirds`] in the world, a flock and a soaring bird ahead of where the
    /// view first looks.
    pub auto_birds: bool,
}

impl Default for AmbientLifeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            motes: true,
            insects: true,
            birds: true,
            leaves: true,
            density_scale: 1.0,
            season: None,
            seed: 0,
            auto_birds: true,
        }
    }
}

/// What a tier may draw, per kind, at full activity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientBudget {
    /// Pollen and dust motes.
    pub motes: u32,
    /// Swarm members (gnats and flies), whole swarms of [`SWARM_MEMBERS`].
    pub swarm_members: u32,
    /// Butterflies.
    pub butterflies: u32,
    /// Fireflies.
    pub fireflies: u32,
    /// Falling leaves.
    pub leaves: u32,
    /// Birds, all flocks together (at most [`MAX_BIRDS`]).
    pub birds: u32,
}

impl AmbientBudget {
    /// Nothing at all: the tiers ambient life is off on.
    pub const OFF: Self = Self {
        motes: 0,
        swarm_members: 0,
        butterflies: 0,
        fireflies: 0,
        leaves: 0,
        birds: 0,
    };

    /// The budget of `tier`: off on Low, Deck and the mobile tiers; Medium is half of High, Ultra
    /// twice High (birds capped at [`MAX_BIRDS`]).
    #[must_use]
    pub fn for_quality(tier: QualityTier) -> Self {
        let row = |motes, swarms: u32, butterflies, fireflies, leaves, birds| Self {
            motes,
            swarm_members: swarms * SWARM_MEMBERS,
            butterflies,
            fireflies,
            leaves,
            birds,
        };
        match tier {
            QualityTier::MobileLow
            | QualityTier::MobileHigh
            | QualityTier::Deck
            | QualityTier::Low
            | QualityTier::VeryLow => Self::OFF,
            QualityTier::Medium => row(1_500, 4, 8, 128, 300, 16),
            QualityTier::High => row(3_000, 8, 16, 256, 600, 32),
            QualityTier::Ultra => row(6_000, 16, 24, 512, 1_200, 64),
        }
    }

    /// `true` when the tier draws nothing.
    #[must_use]
    pub fn is_off(&self) -> bool {
        *self == Self::OFF
    }
}

/// The boxes the stateless kinds live in: `(half extent on X and Z, half height, center height
/// over the eye)`, metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientBox {
    /// Half the box's horizontal size.
    pub half_extent_m: f32,
    /// Half the box's height.
    pub half_height_m: f32,
    /// The box center's height over the eye.
    pub center_y_m: f32,
}

impl AmbientBox {
    /// Motes: close to the eye, where a fleck still reads.
    pub const MOTES: Self = Self::new(7.0, 3.5, 0.0);
    /// Swarm centers: a band around eye height.
    pub const SWARMS: Self = Self::new(11.0, 1.2, -0.3);
    /// Butterflies' home points.
    pub const BUTTERFLIES: Self = Self::new(12.0, 1.0, -0.5);
    /// Fireflies: low over the ground.
    pub const FIREFLIES: Self = Self::new(15.0, 1.4, -0.6);
    /// Leaves: from the canopy above down past the eye.
    pub const LEAVES: Self = Self::new(14.0, 9.0, 4.0);

    const fn new(half_extent_m: f32, half_height_m: f32, center_y_m: f32) -> Self {
        Self {
            half_extent_m,
            half_height_m,
            center_y_m,
        }
    }

    /// The box's size `(2H, 2V, 2H)`.
    #[must_use]
    pub fn size(&self) -> [f32; 3] {
        [
            2.0 * self.half_extent_m,
            2.0 * self.half_height_m,
            2.0 * self.half_extent_m,
        ]
    }

    /// `eye mod size` in `f64`, so the wrap holds far from the origin.
    #[must_use]
    pub fn eye_mod(&self, eye: [f64; 3]) -> [f32; 3] {
        let b = self.size();
        [0, 1, 2].map(|axis| eye[axis].rem_euclid(f64::from(b[axis])) as f32)
    }
}

/// The scene inputs that set how active each kind is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientConditions {
    /// Sine of the sun's elevation (`-1..1`).
    pub sun_elevation_sin: f32,
    /// Rain reaching the ground, mm/h.
    pub rain_mm_per_h: f32,
    /// Snow, water equivalent, mm/h.
    pub snow_mm_per_h: f32,
    /// The wind near the ground on X and Z, m/s.
    pub wind_m_per_s: [f32; 2],
    /// Cloud cover, `0..1`.
    pub cloud_coverage: f32,
    /// The season.
    pub season: Season,
}

impl Default for AmbientConditions {
    fn default() -> Self {
        Self {
            sun_elevation_sin: 0.7,
            rain_mm_per_h: 0.0,
            snow_mm_per_h: 0.0,
            wind_m_per_s: [0.0; 2],
            cloud_coverage: 0.0,
            season: Season::Summer,
        }
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 == edge0 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How active each kind is, `0..1`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AmbientActivity {
    /// Motes: daylight, sun through the cloud, dry and below a fresh breeze; pollen in spring
    /// and summer.
    pub motes: f32,
    /// Swarms: warm season, dry, below a gentle breeze, day and evening.
    pub swarms: f32,
    /// Butterflies: warm season, the sun above 6°, dry, light wind, not overcast.
    pub butterflies: f32,
    /// Fireflies: summer dusk and the first hours of night, dry and calm.
    pub fireflies: f32,
    /// Leaves: autumn (a few in other seasons), more in wind.
    pub leaves: f32,
    /// Birds: day (a few at night), quiet from 1 mm/h of rain and from 12 m/s of wind, the same
    /// limits as the audio's bird calls.
    pub birds: f32,
}

/// The rain at which birds are gone (the audio's bird spots fall quiet at the same rate).
pub const BIRDS_QUIET_RAIN_MM_PER_H: f32 = 1.0;
/// The wind at which birds are gone, m/s (as the audio's bird spots).
pub const BIRDS_QUIET_WIND_M_PER_S: f32 = 12.0;

impl AmbientActivity {
    /// The activity under `conditions`.
    #[must_use]
    pub fn of(conditions: &AmbientConditions) -> Self {
        let s = conditions.sun_elevation_sin;
        let wind = conditions.wind_m_per_s[0].hypot(conditions.wind_m_per_s[1]);
        let calm = |limit: f32| 1.0 - smoothstep(0.5 * limit, limit, wind);
        let rain = conditions.rain_mm_per_h.max(0.0) + conditions.snow_mm_per_h.max(0.0);
        let dry = 1.0 - smoothstep(0.05, 0.6, rain);
        let day = smoothstep(-0.05, 0.15, s);
        let sunny = 1.0 - 0.75 * conditions.cloud_coverage.clamp(0.0, 1.0);
        let (pollen, warm, flutter, glow, fall) = match conditions.season {
            Season::Spring => (1.0, 0.6, 0.6, 0.3, 0.05),
            Season::Summer => (0.8, 1.0, 1.0, 1.0, 0.08),
            Season::Autumn => (0.35, 0.35, 0.25, 0.1, 1.0),
            Season::Winter => (0.15, 0.0, 0.0, 0.0, 0.03),
        };
        // Fireflies flash from just before sunset until the sun is about 17° down.
        let dusk = (1.0 - smoothstep(0.0, 0.09, s)) * smoothstep(-0.3, -0.12, s);
        let birds_dry = 1.0 - smoothstep(0.3 * BIRDS_QUIET_RAIN_MM_PER_H, BIRDS_QUIET_RAIN_MM_PER_H, rain);
        Self {
            motes: day * sunny * dry * calm(9.0) * (0.4 + 0.6 * pollen),
            swarms: warm * dry * calm(6.0) * (0.3 + 0.7 * day),
            butterflies: flutter * smoothstep(0.1, 0.35, s) * dry * calm(6.0) * sunny,
            fireflies: glow * dusk * dry * calm(5.0),
            leaves: fall * (0.6 + 0.4 * (wind / 6.0).min(1.0)) * (1.0 - smoothstep(10.0, 20.0, wind) * 0.3),
            birds: (0.15 + 0.85 * day)
                * birds_dry
                * (1.0 - smoothstep(0.6 * BIRDS_QUIET_WIND_M_PER_S, BIRDS_QUIET_WIND_M_PER_S, wind)),
        }
    }
}

/// The drawn counts: real numbers, so a change fades creatures in and out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AmbientCounts {
    /// Motes.
    pub motes: f32,
    /// Swarm members.
    pub swarm_members: f32,
    /// Butterflies.
    pub butterflies: f32,
    /// Fireflies.
    pub fireflies: f32,
    /// Leaves.
    pub leaves: f32,
    /// Birds.
    pub birds: f32,
}

impl AmbientCounts {
    /// The counts `activity` draws under `budget` and `settings` (a kind switched off draws 0).
    #[must_use]
    pub fn of(activity: &AmbientActivity, settings: &AmbientLifeSettings, budget: &AmbientBudget) -> Self {
        if !settings.enabled {
            return Self::default();
        }
        let scale = settings.density_scale.clamp(0.0, 4.0);
        let count = |on: bool, budget: u32, activity: f32| {
            if on {
                budget as f32 * activity.clamp(0.0, 1.0) * scale
            } else {
                0.0
            }
        };
        let swarms = count(settings.insects, budget.swarm_members, activity.swarms);
        Self {
            motes: count(settings.motes, budget.motes, activity.motes),
            swarm_members: swarms,
            butterflies: count(settings.insects, budget.butterflies, activity.butterflies),
            fireflies: count(settings.insects, budget.fireflies, activity.fireflies),
            leaves: count(settings.leaves, budget.leaves, activity.leaves),
            birds: count(settings.birds, budget.birds.min(MAX_BIRDS as u32), activity.birds)
                .min(MAX_BIRDS as f32),
        }
    }

    /// Instances of a real count: the fade's last creature rounds up.
    #[must_use]
    pub fn instances(count: f32) -> u32 {
        count.ceil().max(0.0) as u32
    }

    /// `true` when nothing is drawn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        Self::instances(self.motes)
            + Self::instances(self.swarm_members)
            + Self::instances(self.butterflies)
            + Self::instances(self.fireflies)
            + Self::instances(self.leaves)
            + Self::instances(self.birds)
            == 0
    }
}

/// Number of leaf fall-speed classes.
pub const LEAF_CLASSES: usize = 4;
/// Terminal fall speed of each leaf class, m/s (a fluttering broad leaf falls at about 1 to
/// 2 m/s).
pub const LEAF_FALL_SPEEDS_M_PER_S: [f32; LEAF_CLASSES] = [0.9, 1.15, 1.4, 1.7];
/// Motes rise this fast in sunlit air, m/s (a slow convective drift).
pub const MOTE_RISE_M_PER_S: f32 = 0.03;
/// The part of the wind motes follow (they sit in the still air under the canopy).
pub const MOTE_WIND_SHARE: f32 = 0.15;
/// The part of the wind leaves follow.
pub const LEAF_WIND_SHARE: f32 = 0.6;

/// Integrates the drifts on game time in `f64`, wrapped to their boxes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AmbientIntegrator {
    /// Mote drift, wrapped to [`AmbientBox::MOTES`].
    pub mote_drift_m: [f64; 3],
    /// Leaf fall per class, wrapped to [`AmbientBox::LEAVES`]'s height.
    pub leaf_fall_m: [f64; LEAF_CLASSES],
    /// Leaf wind drift on X and Z, wrapped.
    pub leaf_drift_m: [f64; 2],
    /// The animation clock, wrapped to [`AMBIENT_LOOP_S`].
    pub clock_s: f64,
}

impl AmbientIntegrator {
    /// Advances by `dt` seconds under `wind` (m/s on X and Z). A non-finite or negative `dt`
    /// does nothing.
    pub fn step(&mut self, wind: [f32; 2], dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let dt = f64::from(dt);
        let wind = wind.map(|w| if w.is_finite() { f64::from(w) } else { 0.0 });
        let mb = AmbientBox::MOTES.size().map(f64::from);
        let lb = AmbientBox::LEAVES.size().map(f64::from);
        let mote_v = [
            wind[0] * f64::from(MOTE_WIND_SHARE),
            f64::from(MOTE_RISE_M_PER_S),
            wind[1] * f64::from(MOTE_WIND_SHARE),
        ];
        for axis in 0..3 {
            self.mote_drift_m[axis] = (self.mote_drift_m[axis] + mote_v[axis] * dt).rem_euclid(mb[axis]);
        }
        for (fall, speed) in self.leaf_fall_m.iter_mut().zip(LEAF_FALL_SPEEDS_M_PER_S) {
            *fall = (*fall + f64::from(speed) * dt).rem_euclid(lb[1]);
        }
        self.leaf_drift_m[0] =
            (self.leaf_drift_m[0] + wind[0] * f64::from(LEAF_WIND_SHARE) * dt).rem_euclid(lb[0]);
        self.leaf_drift_m[1] =
            (self.leaf_drift_m[1] + wind[1] * f64::from(LEAF_WIND_SHARE) * dt).rem_euclid(lb[2]);
        self.clock_s = (self.clock_s + dt).rem_euclid(AMBIENT_LOOP_S);
    }
}

/// The per-frame inputs of the draw, built on the CPU. `Copy` and fixed-size: building it each
/// frame allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientFrame {
    /// World-space eye.
    pub eye: [f64; 3],
    /// The animation clock, `0..AMBIENT_LOOP_S`.
    pub clock_s: f32,
    /// The drawn counts.
    pub counts: AmbientCounts,
    /// The season (the leaf palette).
    pub season: Season,
    /// The seed.
    pub seed: u32,
    /// Mote drift (m).
    pub mote_drift_m: [f32; 3],
    /// Leaf fall per class (m).
    pub leaf_fall_m: [f32; LEAF_CLASSES],
    /// Leaf drift on X and Z (m).
    pub leaf_drift_m: [f32; 2],
    /// `eye mod B` of each box: motes, swarms, butterflies, fireflies, leaves.
    pub eye_mod_m: [[f32; 3]; 5],
    /// The birds, eye-relative; the first `bird_count` are live.
    pub birds: [BirdInstance; MAX_BIRDS],
    /// Live entries of [`Self::birds`].
    pub bird_count: u32,
}

impl Default for AmbientFrame {
    fn default() -> Self {
        Self {
            eye: [0.0; 3],
            clock_s: 0.0,
            counts: AmbientCounts::default(),
            season: Season::Summer,
            seed: 0,
            mote_drift_m: [0.0; 3],
            leaf_fall_m: [0.0; LEAF_CLASSES],
            leaf_drift_m: [0.0; 2],
            eye_mod_m: [[0.0; 3]; 5],
            birds: [BirdInstance::default(); MAX_BIRDS],
            bird_count: 0,
        }
    }
}

impl AmbientFrame {
    /// The frame at `eye` from `integrator`'s drifts.
    #[must_use]
    pub fn new(
        eye: [f64; 3],
        integrator: &AmbientIntegrator,
        counts: AmbientCounts,
        season: Season,
        seed: u32,
    ) -> Self {
        Self {
            eye,
            clock_s: integrator.clock_s as f32,
            counts,
            season,
            seed,
            mote_drift_m: integrator.mote_drift_m.map(|v| v as f32),
            leaf_fall_m: integrator.leaf_fall_m.map(|v| v as f32),
            leaf_drift_m: integrator.leaf_drift_m.map(|v| v as f32),
            eye_mod_m: [
                AmbientBox::MOTES.eye_mod(eye),
                AmbientBox::SWARMS.eye_mod(eye),
                AmbientBox::BUTTERFLIES.eye_mod(eye),
                AmbientBox::FIREFLIES.eye_mod(eye),
                AmbientBox::LEAVES.eye_mod(eye),
            ],
            ..Self::default()
        }
    }

    /// `true` when the frame draws nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty() && self.bird_count == 0
    }
}

/// One creature as the vertex stage places it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientSample {
    /// Eye-relative position, metres.
    pub position_m: [f32; 3],
    /// Size (a mote's or insect's diameter, a wing's or leaf's length), metres.
    pub size_m: f32,
    /// Opacity, `0..1` (0 past the count or outside the box).
    pub alpha: f32,
    /// Emission over the lit color, `0..` (a firefly's flash; 0 for the others).
    pub glow: f32,
}

/// Regia's integer hash: the same bits as `ambient_hash` in `ambient.wgsl`.
fn hash_step(state: u32) -> u32 {
    let word = state.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let mixed = ((word >> ((word >> 28) + 4)) ^ word).wrapping_mul(277_803_737);
    (mixed >> 22) ^ mixed
}

fn unit(bits: u32) -> f32 {
    (bits >> 8) as f32 / 16_777_216.0
}

/// Four values in `[0, 1)` from `index` in the stream of `salt` and `seed`.
#[must_use]
pub fn ambient_hash4(index: u32, salt: u32, seed: u32) -> [f32; 4] {
    let a = hash_step(index ^ salt ^ hash_step(seed));
    let b = hash_step(a);
    let c = hash_step(b);
    let d = hash_step(c);
    [unit(a), unit(b), unit(c), unit(d)]
}

fn fract(x: f32) -> f32 {
    x - x.floor()
}

/// Whole cycles per loop for a frequency in `lo..hi` Hz picked by `h` (`0..1`), as Hz.
fn looped_hz(lo: f32, hi: f32, h: f32) -> f32 {
    let loop_s = AMBIENT_LOOP_S as f32;
    ((lo + (hi - lo) * h) * loop_s).round().max(1.0) / loop_s
}

/// A creature's place in a wrapped box: `(eye-relative position, edge fade)`.
fn wrapped(seed: [f32; 4], offset: [f32; 3], bx: &AmbientBox, eye_mod: [f32; 3]) -> ([f32; 3], f32) {
    let b = bx.size();
    let shift = [0.0, bx.center_y_m, 0.0];
    let half = [bx.half_extent_m, bx.half_height_m, bx.half_extent_m];
    let mut position = [0.0; 3];
    let mut edge = 0.0_f32;
    for axis in 0..3 {
        let x = b[axis] * seed[axis] + offset[axis];
        let c = eye_mod[axis] + shift[axis];
        let local = b[axis] * fract((x - c) / b[axis]) - half[axis];
        position[axis] = shift[axis] + local;
        edge = edge.max(local.abs() / half[axis]);
    }
    (position, 1.0 - smoothstep(1.0 - EDGE_FADE, 1.0, edge))
}

fn count_fade(index: u32, count: f32) -> f32 {
    if count <= 0.0 {
        return 0.0;
    }
    // The second term fades the first creatures in as well: alone, the first shows whole at any
    // count over 0.
    (1.0 - smoothstep((1.0 - COUNT_FADE) * count, count, index as f32))
        .min((count - index as f32).clamp(0.0, 1.0))
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// CPU mirror of `vs_mote`: mote `index` of `frame`. Pollen and dust, 2.5 to 8 mm as a lens sees
/// them, drifting with a slow wobble.
#[must_use]
pub fn mote_at(index: u32, frame: &AmbientFrame) -> AmbientSample {
    let h = ambient_hash4(index, SALT_MOTE, frame.seed);
    let (base, fade) = wrapped(h, frame.mote_drift_m, &AmbientBox::MOTES, frame.eye_mod_m[0]);
    let t = frame.clock_s;
    let f = looped_hz(0.08, 0.2, h[3]);
    let a = TAU * (f * t + h[0]);
    let wobble = [
        0.08 * a.sin(),
        0.05 * (2.0 * a + h[1] * TAU).sin(),
        0.08 * a.cos(),
    ];
    AmbientSample {
        position_m: add(base, wobble),
        size_m: 0.0025 + 0.0055 * h[3] * h[3],
        alpha: fade * count_fade(index, frame.counts.motes),
        glow: 0.0,
    }
}

/// `true` when swarm `swarm` of `frame`'s seed is a fly swarm (a few larger, wider-ranging
/// insects) rather than gnats.
#[must_use]
pub fn swarm_is_flies(swarm: u32, seed: u32) -> bool {
    ambient_hash4(swarm, SALT_SWARM, seed)[3] > 0.7
}

/// CPU mirror of `vs_swarm`: member `index` (swarm `index / SWARM_MEMBERS`). Gnats dance in a
/// tight ball about 0.5 m across; flies loop wider and faster.
#[must_use]
pub fn swarm_member_at(index: u32, frame: &AmbientFrame) -> AmbientSample {
    let swarm = index / SWARM_MEMBERS;
    let member = index % SWARM_MEMBERS;
    let hs = ambient_hash4(swarm, SALT_SWARM, frame.seed);
    let flies = hs[3] > 0.7;
    let (center, fade) = wrapped(hs, [0.0; 3], &AmbientBox::SWARMS, frame.eye_mod_m[1]);
    let hm = ambient_hash4(index, SALT_MEMBER, frame.seed);
    let t = frame.clock_s;
    let (radius, lo, hi, size) = if flies {
        (1.4, 0.4, 0.9, 0.009)
    } else {
        (0.3 + 0.25 * hm[3], 0.6, 1.6, 0.003)
    };
    let ax = TAU * (looped_hz(lo, hi, hm[0]) * t + hm[1]);
    let ay = TAU * (looped_hz(lo, hi, hm[1]) * t + hm[2]);
    let az = TAU * (looped_hz(lo, hi, hm[2]) * t + hm[0]);
    let offset = [radius * ax.sin(), 0.5 * radius * ay.sin(), radius * az.sin()];
    let live = if flies { member < FLY_SWARM_MEMBERS } else { true };
    let alpha = if live {
        fade * count_fade(index, frame.counts.swarm_members)
    } else {
        0.0
    };
    AmbientSample {
        position_m: add(center, offset),
        size_m: size,
        alpha,
        glow: 0.0,
    }
}

/// A butterfly's meander around its home point: `(offset, velocity)`.
fn butterfly_path(h: [f32; 4], t: f32) -> ([f32; 3], [f32; 3]) {
    let f1 = looped_hz(0.05, 0.11, h[0]);
    let f2 = looped_hz(0.13, 0.23, h[1]);
    let fy = looped_hz(0.3, 0.6, h[2]);
    let a1 = TAU * (f1 * t + h[1]);
    let a2 = TAU * (f2 * t + h[2]);
    let ay = TAU * (fy * t + h[3]);
    let (r1, r2, ry) = (3.0, 1.2, 0.35);
    let offset = [
        r1 * a1.sin() + r2 * a2.cos(),
        ry * ay.sin(),
        r1 * a1.cos() + r2 * a2.sin(),
    ];
    let velocity = [
        TAU * (r1 * f1 * a1.cos() - r2 * f2 * a2.sin()),
        TAU * ry * fy * ay.cos(),
        TAU * (-r1 * f1 * a1.sin() + r2 * f2 * a2.cos()),
    ];
    (offset, velocity)
}

/// CPU mirror of `vs_butterfly`'s placement: butterfly `index`, a wing 3 to 5 cm long.
#[must_use]
pub fn butterfly_at(index: u32, frame: &AmbientFrame) -> AmbientSample {
    let h = ambient_hash4(index, SALT_BUTTERFLY, frame.seed);
    let (home, fade) = wrapped(h, [0.0; 3], &AmbientBox::BUTTERFLIES, frame.eye_mod_m[2]);
    let (offset, _) = butterfly_path(h, frame.clock_s);
    AmbientSample {
        position_m: add(home, offset),
        size_m: 0.03 + 0.02 * h[3],
        alpha: fade * count_fade(index, frame.counts.butterflies),
        glow: 0.0,
    }
}

/// A butterfly's wing angle over the body plane, radians: flaps at 5 to 9 Hz, with glides.
#[must_use]
pub fn butterfly_wing_angle(index: u32, frame: &AmbientFrame) -> f32 {
    let h = ambient_hash4(index, SALT_BUTTERFLY, frame.seed);
    let t = frame.clock_s;
    let flap = (TAU * (looped_hz(5.0, 9.0, h[2]) * t + h[0])).sin();
    // About a third of the time it glides with the wings half open.
    let glide = smoothstep(0.55, 0.75, (TAU * (looped_hz(0.2, 0.4, h[1]) * t + h[3])).sin());
    let flapping = 0.15 + 0.6 * (1.0 + flap);
    flapping * (1.0 - glide) + 0.35 * glide
}

/// The firefly flash at `phase` (`0..1` of its period): a half-second rise and fall early in the
/// cycle, dark the rest.
fn flash(phase: f32, period_s: f32) -> f32 {
    let on = 0.3 / period_s;
    let rise = 0.08 / period_s;
    smoothstep(0.0, rise, phase) * (1.0 - smoothstep(on, on + 0.2 / period_s, phase))
}

/// CPU mirror of `vs_firefly`: firefly `index`, flashing every 2.5 to 6 s.
#[must_use]
pub fn firefly_at(index: u32, frame: &AmbientFrame) -> AmbientSample {
    let h = ambient_hash4(index, SALT_FIREFLY, frame.seed);
    let (base, fade) = wrapped(h, [0.0; 3], &AmbientBox::FIREFLIES, frame.eye_mod_m[3]);
    let t = frame.clock_s;
    let a = TAU * (looped_hz(0.03, 0.08, h[3]) * t + h[0]);
    let drift = [0.6 * a.sin(), 0.25 * (2.0 * a).sin(), 0.6 * a.cos()];
    let f = looped_hz(1.0 / 6.0, 1.0 / 2.5, h[3]);
    let phase = fract(f * t + h[1]);
    AmbientSample {
        position_m: add(base, drift),
        size_m: 0.06,
        alpha: fade * count_fade(index, frame.counts.fireflies),
        glow: flash(phase, 1.0 / f),
    }
}

/// CPU mirror of `vs_leaf`'s placement: leaf `index`, 4 to 7 cm, falling at its class's speed
/// with the zig-zag sway of a falling broad leaf and the wind's drift.
#[must_use]
pub fn leaf_at(index: u32, frame: &AmbientFrame) -> AmbientSample {
    let k = index as usize % LEAF_CLASSES;
    let h = ambient_hash4(index, SALT_LEAF, frame.seed);
    let offset = [
        frame.leaf_drift_m[0],
        -frame.leaf_fall_m[k],
        frame.leaf_drift_m[1],
    ];
    let (base, fade) = wrapped(h, offset, &AmbientBox::LEAVES, frame.eye_mod_m[4]);
    let t = frame.clock_s;
    let sway = 0.3 + 0.4 * h[3];
    let a = TAU * (looped_hz(0.35, 0.8, h[0]) * t + h[1]);
    let dir = TAU * h[2];
    let s = sway * a.sin();
    let position = add(
        base,
        [s * dir.cos(), 0.15 * sway * (2.0 * a).cos(), s * dir.sin()],
    );
    AmbientSample {
        position_m: position,
        size_m: 0.04 + 0.03 * h[3],
        alpha: fade * count_fade(index, frame.counts.leaves),
        glow: 0.0,
    }
}

#[cfg(test)]
mod tests;
