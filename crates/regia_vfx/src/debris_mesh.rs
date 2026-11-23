// SPDX-License-Identifier: MPL-2.0
//! Mesh debris: glTF fragments thrown by an effect, flown on ballistic arcs and settled on the
//! ground, every pose a pure function of the frame number.
//!
//! A [`DebrisSpec`] names the classes of fragment (earth, stone, turf, hardware), how many of
//! each, how fast they leave the origin and how long they live. [`build_fragments`] turns the
//! spec and a ground-height function into a list of [`Fragment`]s once, when the burst is
//! created: each fragment's launch velocity, spin, landing time and resting place are fixed
//! then. After that [`Fragment::pose`] is a closed-form function of the time since the burst
//! began, so the same seed, spec and ground give the same arcs on every run, on every machine,
//! and a seek or a replay needs no simulation state (`docs/specs/23-agent-native.md`).
//!
//! **Flight.** Gravity plus optional linear drag `k` (1/s), solved in closed form (no
//! integration step). With `k = 0` this is the plain parabola `p₀ + v₀t + ½gt²` (the airless
//! case). **Landing.** The landing time is found once at build time by marching the closed form
//! in fixed 1/30 s steps and bisecting the step that crosses the ground. **Settling.** There is
//! no bounce and no collision: an inelastic touch-down keeps 0.35 of the horizontal landing
//! speed, and the fragment then slides the Coulomb distance of that speed,
//! `(0.35 v)² / (2 μ g)` (μ floored at 0.05), capped at [`MAX_SLIDE_M`]. Its tumble eases to a
//! resting orientation, and it rests partly sunk by its class's burial fraction. **Heat.** Only fragments flagged molten at launch glow,
//! and the glow decays with a time constant of well under a second ([`MOLTEN_TAU_S`]); nothing
//! trails flame. **Cost.** Per frame per fragment: one closed-form pose and one transform
//! write.
//!
//! The ECS side ([`DebrisPlugin`]) spawns one entity per live fragment (a
//! [`regia_render::MeshRenderer`] sharing one mesh and material per variant, so the renderer
//! batches them) and rewrites the transforms from [`DebrisClock`]. A fragment past its lifetime
//! is despawned; rewinding the clock respawns it.
//!
//! **Tiers.** [`fragment_budget`] caps the fragment count per [`QualityTier`]; the cheap detail
//! level (Low, Deck, Mobile) drops the molten glow and the shrink fade. When the cap bites,
//! hardware keeps at most a quarter of the budget and the other classes share the rest in
//! proportion to what they asked for, which can raise a class above that count. An all-hardware
//! burst stops at that quarter and leaves the rest of the budget empty.
//!
//! Provenance: ballistics with linear drag is the textbook closed form; the slide distance is
//! the Coulomb friction stopping distance; the 1/γ-biased speed law is original
//! (`docs/provenance.md`).

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_acos, det_exp, det_exp_f64, det_sin_cos};
use regia_render::quality::QualityTier;
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer};
use regia_scene::Transform;

/// A quaternion rotating `angle` radians about the unit `axis`, from the deterministic
/// [`det_sin_cos`] pair: the same bits on every machine, unlike glam's `from_axis_angle`
/// (platform libm `sin_cos`). Every orientation that feeds [`poses_hash`] builds on this.
fn det_axis_angle(axis: Vec3, angle: f32) -> Quat {
    let (sin, cos) = det_sin_cos(angle * 0.5);
    Quat::from_xyzw(axis.x * sin, axis.y * sin, axis.z * sin, cos)
}

/// `a.slerp(b, t)` on the deterministic kernels ([`det_acos`], [`det_sin_cos`]): the same
/// orientation blend on every machine. Mirrors glam's shortest-path convention.
fn det_slerp(a: Quat, b: Quat, t: f32) -> Quat {
    let d = a.dot(b);
    let (b, d) = if d < 0.0 { (-b, -d) } else { (b, d) };
    if d > 0.9995 {
        return (a + (b - a) * t).normalize();
    }
    let omega = det_acos(d.clamp(-1.0, 1.0));
    // sin((1-t)ω)/sin(ω) = cos(tω) − d·sin(tω)/sin(ω), with sin(ω) = sqrt(1−d²).
    let sin_omega = (1.0 - d * d).max(0.0).sqrt();
    let (sin, cos) = det_sin_cos(t * omega);
    let k1 = sin / sin_omega;
    let k0 = cos - d * k1;
    a * k0 + b * k1
}

/// Time constant of the molten glow, in seconds: a fragment flagged molten glows `e^(-t/τ)` of
/// its launch heat and is dark after [`MOLTEN_MAX_S`].
pub const MOLTEN_TAU_S: f32 = 0.7;
/// A molten fragment is fully dark from this age on, in seconds.
pub const MOLTEN_MAX_S: f32 = 3.0;
/// Longest flight the landing search follows, in seconds (a fragment still up then rests at the
/// point it reached).
pub const MAX_FLIGHT_S: f32 = 90.0;
/// Most bursts alive at once (a request past this is refused).
pub const MAX_BURSTS: usize = 8;
/// Most fragments one burst may hold, whatever the spec asks for.
pub const MAX_FRAGMENTS: u32 = 8192;
/// Furthest a landed fragment slides, in metres.
pub const MAX_SLIDE_M: f32 = 6.0;
/// Frame rate the pose time is measured at unless the spec says otherwise.
pub const DEFAULT_FPS: f32 = 60.0;

/// What a fragment is made of (picks its speed, size, drag, burial and mesh set).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DebrisClass {
    /// Clods and chunks of soil.
    Earth,
    /// Rock and gravel.
    Stone,
    /// Sod with the grass or snow crust still on.
    Turf,
    /// Plates and shards of the machinery that arrived.
    Hardware,
}

impl DebrisClass {
    /// Every class, in a fixed order (the build order of fragments).
    pub const ALL: [Self; 4] = [Self::Earth, Self::Stone, Self::Turf, Self::Hardware];

    /// Lower-case name (`earth`, `stone`, `turf`, `hardware`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Earth => "earth",
            Self::Stone => "stone",
            Self::Turf => "turf",
            Self::Hardware => "hardware",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    fn index(self) -> u64 {
        match self {
            Self::Earth => 0,
            Self::Stone => 1,
            Self::Turf => 2,
            Self::Hardware => 3,
        }
    }
}

/// One class of fragment in a burst.
#[derive(Clone, Debug, PartialEq)]
pub struct DebrisClassSpec {
    /// The class.
    pub class: DebrisClass,
    /// How many fragments of this class (before the tier cap).
    pub count: u32,
    /// Launch speed range, m/s. The speed is `lo + (hi - lo)·u²` for a uniform `u`, so most
    /// fragments are slow and a few fly far.
    pub speed: [f32; 2],
    /// Fragment size range in metres (a unit-size mesh is scaled to this).
    pub size: [f32; 2],
    /// Linear drag, 1/s (`0` = airless).
    pub drag: f32,
    /// Seconds a fragment lives (`None`: until the burst is removed).
    pub lifetime_s: Option<f32>,
    /// Fraction of the fragment sunk below the ground when it rests, `0..=0.5`.
    pub burial: f32,
    /// Fraction of fragments flagged molten at launch (they glow briefly).
    pub molten_fraction: f32,
    /// How many mesh variants of this class the host registered (`mesh_slot` is taken modulo
    /// the real count; this only spreads the hash).
    pub variants: u32,
}

impl DebrisClassSpec {
    /// Soil clods thrown by a blast.
    #[must_use]
    pub fn earth(count: u32) -> Self {
        Self {
            class: DebrisClass::Earth,
            count,
            speed: [8.0, 95.0],
            size: [0.15, 0.9],
            drag: 0.02,
            lifetime_s: Some(240.0),
            burial: 0.35,
            molten_fraction: 0.0,
            variants: 4,
        }
    }

    /// Rock thrown by a blast.
    #[must_use]
    pub fn stone(count: u32) -> Self {
        Self {
            class: DebrisClass::Stone,
            count,
            speed: [10.0, 100.0],
            size: [0.1, 0.7],
            drag: 0.005,
            lifetime_s: Some(300.0),
            burial: 0.25,
            molten_fraction: 0.0,
            variants: 4,
        }
    }

    /// Sod and crust thrown by a blast.
    #[must_use]
    pub fn turf(count: u32) -> Self {
        Self {
            class: DebrisClass::Turf,
            count,
            speed: [6.0, 70.0],
            size: [0.3, 1.4],
            drag: 0.04,
            lifetime_s: Some(240.0),
            burial: 0.4,
            molten_fraction: 0.0,
            variants: 3,
        }
    }

    /// Plates and shards of hardware; they stay (no lifetime) and some start molten.
    #[must_use]
    pub fn hardware(count: u32) -> Self {
        Self {
            class: DebrisClass::Hardware,
            count,
            speed: [10.0, 60.0],
            size: [0.6, 2.4],
            drag: 0.004,
            lifetime_s: None,
            burial: 0.2,
            molten_fraction: 0.4,
            variants: 4,
        }
    }
}

/// A burst of debris: where, when, how much, from what seed.
#[derive(Clone, Debug, PartialEq)]
pub struct DebrisSpec {
    /// Launch point (world metres), usually the impact point.
    pub origin: Vec3,
    /// Seed for every random draw.
    pub seed: u64,
    /// Frame (on [`DebrisClock`]) the burst begins.
    pub start_frame: u64,
    /// Frames per second the clock runs at.
    pub fps: f32,
    /// Gravity, m/s² (9.81 on Earth, 3.71 on Mars, 1.62 on the Moon).
    pub gravity: f32,
    /// Launch elevation range above the horizontal, radians.
    pub elevation: [f32; 2],
    /// Seconds a fragment shrinks away over before its lifetime ends (`0`: it pops out).
    pub fade_s: f32,
    /// Friction coefficient of the ground a fragment slides on after landing.
    pub friction: f32,
    /// The classes.
    pub classes: Vec<DebrisClassSpec>,
    /// Cheap detail: no molten glow, no shrink fade (Low, Deck, Mobile).
    pub cheap: bool,
}

impl DebrisSpec {
    /// An earth/stone/turf blast with hardware plates: 1236 fragments, 36 of them plates (the
    /// showcase's "1000+ pieces plus 20–50 plates"). Apex and range follow from the speeds:
    /// 95 m/s at 80° reaches about 440 m up, 100 m/s at 45° lands about 1 km out.
    #[must_use]
    pub fn earth_blast(origin: Vec3, seed: u64, start_frame: u64) -> Self {
        Self {
            origin,
            seed,
            start_frame,
            fps: DEFAULT_FPS,
            gravity: 9.81,
            elevation: [0.45, 1.45],
            fade_s: 6.0,
            friction: 0.6,
            classes: vec![
                DebrisClassSpec::earth(520),
                DebrisClassSpec::stone(380),
                DebrisClassSpec::turf(300),
                DebrisClassSpec::hardware(36),
            ],
            cheap: false,
        }
    }

    /// Total fragments asked for (before the tier cap).
    #[must_use]
    pub fn total(&self) -> u32 {
        self.classes
            .iter()
            .fold(0u32, |acc, c| acc.saturating_add(c.count))
    }

    /// Caps the fragment count at `max` (and at [`MAX_FRAGMENTS`]).
    ///
    /// Hardware is not filled first. It keeps at most `max / 4` plates, shared across hardware
    /// classes in proportion to what they asked for. The other classes share
    /// `max - kept_hardware` the same way, which can raise a class above the count the host set.
    /// An all-hardware burst stops at `max / 4` and leaves the rest empty. A burst already within
    /// `max` is unchanged. Integer division can leave the total a few under `max`.
    pub fn apply_budget(&mut self, max: u32) {
        let max = max.min(MAX_FRAGMENTS);
        if self.total() <= max {
            return;
        }
        // Summed in u64: class counts the host set can add up past `u32::MAX` (corr-vfx L3).
        let (hardware, others) = self.classes.iter().fold((0u64, 0u64), |(h, o), c| {
            if c.class == DebrisClass::Hardware {
                (h + u64::from(c.count), o)
            } else {
                (h, o + u64::from(c.count))
            }
        });
        let kept_hardware = hardware.min(u64::from(max / 4));
        let room = u64::from(max) - kept_hardware;
        for c in &mut self.classes {
            // Each share is at most `max`, so it fits a u32. A class kind with nothing asked for
            // has no shares to take: hardware drops to 0, the others keep their count.
            if c.class == DebrisClass::Hardware {
                c.count = (u64::from(c.count) * kept_hardware)
                    .checked_div(hardware)
                    .unwrap_or(0) as u32;
            } else if let Some(share) = (u64::from(c.count) * room).checked_div(others) {
                c.count = share as u32;
            }
        }
    }

    /// Applies a tier: the fragment cap and the detail level.
    pub fn apply_tier(&mut self, tier: QualityTier) {
        self.apply_budget(fragment_budget(tier));
        self.cheap = cheap_detail(tier);
    }
}

/// Most fragments a burst may hold on `tier`.
#[must_use]
pub const fn fragment_budget(tier: QualityTier) -> u32 {
    match tier {
        QualityTier::MobileLow => 48,
        QualityTier::MobileHigh => 128,
        QualityTier::Deck => 192,
        QualityTier::Low | QualityTier::VeryLow => 256,
        QualityTier::Medium => 768,
        QualityTier::High => 1536,
        QualityTier::Ultra => 4096,
    }
}

/// Whether `tier` uses the cheap detail level (no molten glow, no shrink fade).
#[must_use]
pub const fn cheap_detail(tier: QualityTier) -> bool {
    matches!(
        tier,
        QualityTier::MobileLow | QualityTier::MobileHigh | QualityTier::Deck | QualityTier::Low
    )
}

/// One fragment, fully determined at build time.
#[derive(Clone, Debug, PartialEq)]
pub struct Fragment {
    /// The class.
    pub class: DebrisClass,
    /// Index within the class (stable across tier caps: the first `n` of a class are kept).
    pub index: u32,
    /// Hash that picks the mesh variant (`mesh_slot % variants`).
    pub mesh_slot: u32,
    /// Size in metres.
    pub size: f32,
    /// Launch point.
    pub p0: Vec3,
    /// Launch velocity.
    pub v0: Vec3,
    /// Gravity, m/s².
    pub gravity: f32,
    /// Linear drag, 1/s.
    pub drag: f32,
    /// Tumble axis (unit).
    pub spin_axis: Vec3,
    /// Tumble rate, rad/s.
    pub spin_rate: f32,
    /// Orientation at launch.
    pub rot0: Quat,
    /// Time of first ground contact, seconds.
    pub land_t: f32,
    /// Where it touches down.
    pub land_pos: Vec3,
    /// Orientation at touch-down.
    pub land_rot: Quat,
    /// Where it comes to rest.
    pub rest_pos: Vec3,
    /// Resting orientation.
    pub rest_rot: Quat,
    /// Slide speed at touch-down, m/s.
    pub slide_speed: f32,
    /// Slide deceleration, m/s².
    pub slide_decel: f32,
    /// Total slide distance, metres.
    pub slide_len: f32,
    /// Seconds the tumble takes to ease to rest after touch-down.
    pub settle_s: f32,
    /// Lifetime in seconds (`f32::INFINITY`: stays).
    pub lifetime_s: f32,
    /// Seconds the fragment shrinks over at the end of its life.
    pub fade_s: f32,
    /// Whether it starts molten (glows briefly).
    pub molten: bool,
}

/// A fragment's pose at one instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FragmentPose {
    /// World position of the fragment's centre.
    pub translation: Vec3,
    /// Orientation.
    pub rotation: Quat,
    /// Uniform scale in metres (the size, shrunk during the fade).
    pub scale: f32,
    /// Molten glow `0..=1` (`0` once cooled; always `0` at cheap detail).
    pub heat: f32,
    /// Whether the fragment has touched down.
    pub landed: bool,
}

#[derive(Clone, Copy)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        // splitmix64 (public domain, Steele/Lea/Flood).
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Position at time `t` of a body launched from `p0` with velocity `v0` under gravity `g`
/// (along −Y) and linear drag `k` (1/s). Closed form; `k ≈ 0` is the parabola.
#[must_use]
pub fn flight_position(p0: Vec3, v0: Vec3, g: f32, k: f32, t: f32) -> Vec3 {
    if k.abs() < 1e-6 {
        return p0 + v0 * t + Vec3::new(0.0, -0.5 * g * t * t, 0.0);
    }
    // f64 inside: `g / k` is large for a small drag and the terms cancel. The exponential is
    // the deterministic kernel, not libm: the flight path feeds poses_hash, which must agree
    // on every machine (fix-vfx1-determinism finding 3).
    let (k, t, g) = (f64::from(k), f64::from(t), f64::from(g));
    let decay = det_exp_f64(-k * t);
    let along = (1.0 - decay) / k;
    let x = f64::from(p0.x) + f64::from(v0.x) * along;
    let z = f64::from(p0.z) + f64::from(v0.z) * along;
    let y = f64::from(p0.y) + (f64::from(v0.y) + g / k) * along - g * t / k;
    Vec3::new(x as f32, y as f32, z as f32)
}

/// Velocity at time `t` for the same flight as [`flight_position`].
#[must_use]
pub fn flight_velocity(v0: Vec3, g: f32, k: f32, t: f32) -> Vec3 {
    if k.abs() < 1e-6 {
        return Vec3::new(v0.x, v0.y - g * t, v0.z);
    }
    let (k, t, g) = (f64::from(k), f64::from(t), f64::from(g));
    let decay = det_exp_f64(-k * t);
    Vec3::new(
        (f64::from(v0.x) * decay) as f32,
        ((f64::from(v0.y) + g / k) * decay - g / k) as f32,
        (f64::from(v0.z) * decay) as f32,
    )
}

/// Builds the fragments of `spec`, finding each landing against `ground` (height of the ground
/// at world `(x, z)`).
///
/// Deterministic: the draws for fragment `i` of a class depend only on the seed, the class and
/// `i`, so lowering a count (a tier cap) keeps the arcs of the fragments that remain.
#[must_use]
pub fn build_fragments(spec: &DebrisSpec, ground: &dyn Fn(f32, f32) -> f32) -> Vec<Fragment> {
    let _span = tracing::info_span!("debris_build", classes = spec.classes.len()).entered();
    let mut out = Vec::with_capacity(spec.total().min(MAX_FRAGMENTS) as usize);
    for class_spec in &spec.classes {
        for index in 0..class_spec.count {
            if out.len() >= MAX_FRAGMENTS as usize {
                return out;
            }
            out.push(build_one(spec, class_spec, index, ground));
        }
    }
    out
}

fn build_one(
    spec: &DebrisSpec,
    class_spec: &DebrisClassSpec,
    index: u32,
    ground: &dyn Fn(f32, f32) -> f32,
) -> Fragment {
    let mut rng = Rng::new(
        spec.seed ^ (class_spec.class.index() << 56) ^ u64::from(index).wrapping_mul(0xd6e8_feb8_6659_fd93),
    );
    let azimuth = rng.range(0.0, std::f32::consts::TAU);
    let elevation = rng.range(spec.elevation[0], spec.elevation[1]);
    let u = rng.unit();
    let speed = class_spec.speed[0] + (class_spec.speed[1] - class_spec.speed[0]) * u * u;
    let size = rng.range(class_spec.size[0], class_spec.size[1]);
    // The launch direction from the deterministic sin/cos pair: the arcs (and poses_hash)
    // must be the same on every machine, not follow the platform libm (finding 3).
    let (az_sin, az_cos) = det_sin_cos(azimuth);
    let (el_sin, el_cos) = det_sin_cos(elevation);
    let dir = Vec3::new(az_cos * el_cos, el_sin, az_sin * el_cos);
    let v0 = dir * speed;
    let axis = Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
    let spin_axis = if axis.length_squared() > 1e-6 {
        axis.normalize()
    } else {
        Vec3::X
    };
    let spin_rate = rng.range(-6.0, 6.0);
    let rot0 = det_axis_angle(
        spin_axis.any_orthonormal_vector(),
        rng.range(0.0, std::f32::consts::TAU),
    );
    let molten = class_spec.molten_fraction > 0.0 && rng.unit() < class_spec.molten_fraction;
    let mesh_slot = (rng.next_u64() >> 32) as u32;
    let yaw = rng.range(0.0, std::f32::consts::TAU);
    let tilt_x = rng.range(-0.25, 0.25);
    let tilt_z = rng.range(-0.25, 0.25);
    let rest_rot =
        det_axis_angle(Vec3::Y, yaw) * det_axis_angle(Vec3::X, tilt_x) * det_axis_angle(Vec3::Z, tilt_z);
    let burial = class_spec.burial.clamp(0.0, 0.5);
    let rest_offset = size * 0.5 * (1.0 - 2.0 * burial);
    let (g, k) = (spec.gravity, class_spec.drag.max(0.0));

    // Landing: march the closed form, test only on the way down, then bisect the crossing.
    let below = |t: f32| {
        let p = flight_position(spec.origin, v0, g, k, t);
        p.y <= ground(p.x, p.z) + rest_offset
    };
    let descending = |t: f32| flight_velocity(v0, g, k, t).y < 0.0;
    let step = 1.0 / 30.0;
    let mut t = 0.0f32;
    let mut land_t = MAX_FLIGHT_S;
    while t < MAX_FLIGHT_S {
        let next = t + step;
        if descending(next) && below(next) {
            let (mut lo, mut hi) = (t, next);
            for _ in 0..20 {
                let mid = 0.5 * (lo + hi);
                if below(mid) && descending(mid) {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            land_t = hi;
            break;
        }
        t = next;
    }
    let flight = flight_position(spec.origin, v0, g, k, land_t);
    let land_pos = Vec3::new(flight.x, ground(flight.x, flight.z) + rest_offset, flight.z);
    let land_vel = flight_velocity(v0, g, k, land_t);
    let horizontal = Vec3::new(land_vel.x, 0.0, land_vel.z);
    // An inelastic touch-down keeps about a third of the horizontal speed for the slide.
    let slide_dir = if horizontal.length_squared() > 1e-8 {
        horizontal.normalize()
    } else {
        Vec3::ZERO
    };
    let decel = (spec.friction.max(0.05) * g).max(1e-3);
    let mut slide_speed = horizontal.length() * 0.35;
    let mut slide_len = slide_speed * slide_speed / (2.0 * decel);
    if slide_len > MAX_SLIDE_M {
        slide_len = MAX_SLIDE_M;
        slide_speed = (2.0 * decel * slide_len).sqrt();
    }
    let rest_xz = land_pos + slide_dir * slide_len;
    let rest_pos = Vec3::new(rest_xz.x, ground(rest_xz.x, rest_xz.z) + rest_offset, rest_xz.z);
    let land_rot = det_axis_angle(spin_axis, spin_rate * land_t) * rot0;
    let lifetime_s = class_spec.lifetime_s.map_or(f32::INFINITY, |l| l.max(0.0));
    Fragment {
        class: class_spec.class,
        index,
        mesh_slot,
        size,
        p0: spec.origin,
        v0,
        gravity: g,
        drag: k,
        spin_axis,
        spin_rate,
        rot0,
        land_t,
        land_pos,
        land_rot,
        rest_pos,
        rest_rot,
        slide_speed,
        slide_decel: decel,
        slide_len,
        settle_s: 0.4 + slide_speed / decel,
        lifetime_s,
        fade_s: if spec.cheap { 0.0 } else { spec.fade_s.max(0.0) },
        molten: molten && !spec.cheap,
    }
}

impl Fragment {
    /// The pose `t` seconds after the burst began, or `None` before it begins and after the
    /// fragment's lifetime. A pure function of the fragment and `t`.
    #[must_use]
    pub fn pose(&self, t: f32) -> Option<FragmentPose> {
        if t < 0.0 || t >= self.lifetime_s {
            return None;
        }
        let (translation, rotation, landed) = if t < self.land_t {
            let p = flight_position(self.p0, self.v0, self.gravity, self.drag, t);
            let r = det_axis_angle(self.spin_axis, self.spin_rate * t) * self.rot0;
            (p, r, false)
        } else {
            let ts = t - self.land_t;
            let slid = if self.slide_decel > 0.0 && ts * self.slide_decel < self.slide_speed {
                self.slide_speed * ts - 0.5 * self.slide_decel * ts * ts
            } else {
                self.slide_len
            };
            let frac = if self.slide_len > 1e-6 {
                (slid / self.slide_len).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let p = self.land_pos.lerp(self.rest_pos, frac);
            let ease = smoothstep(0.0, self.settle_s, ts);
            (p, det_slerp(self.land_rot, self.rest_rot, ease), true)
        };
        let shrink = if self.fade_s > 0.0 && self.lifetime_s.is_finite() {
            1.0 - smoothstep(self.lifetime_s - self.fade_s, self.lifetime_s, t)
        } else {
            1.0
        };
        let heat = if self.molten && t < MOLTEN_MAX_S {
            det_exp(-t / MOLTEN_TAU_S)
        } else {
            0.0
        };
        Some(FragmentPose {
            translation,
            rotation,
            scale: self.size * shrink,
            heat,
            landed,
        })
    }
}

/// Seconds since a burst began at `start_frame`, for `frame` on a clock of `fps` (negative
/// before the start). The frame delta divides in `f64`: past 2^24 frames an `f32` delta
/// cannot represent adjacent frames — at 30 fps, frames 20,000,000 and 20,000,001 both give
/// `666666.7` — so the returned time stops resolving individual frames there (the f64 division
/// keeps the value honest; only the `f32` result quantizes, fix-vfx1-determinism finding 8).
/// Fragments have landed long before, so nothing observable depends on the last digits.
#[must_use]
pub fn burst_time(frame: u64, start_frame: u64, fps: f32) -> f32 {
    let fps = if fps > 0.0 { fps } else { DEFAULT_FPS };
    ((frame as f64 - start_frame as f64) / f64::from(fps)) as f32
}

/// FNV-1a over the pose bits of every fragment at `frame`: equal hashes mean equal arcs.
#[must_use]
pub fn poses_hash(fragments: &[Fragment], start_frame: u64, fps: f32, frame: u64) -> u64 {
    let t = burst_time(frame, start_frame, fps);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bits: u32| {
        for b in bits.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    for f in fragments {
        match f.pose(t) {
            Some(p) => {
                for v in [
                    p.translation.x,
                    p.translation.y,
                    p.translation.z,
                    p.rotation.x,
                    p.rotation.y,
                    p.rotation.z,
                    p.rotation.w,
                    p.scale,
                    p.heat,
                ] {
                    eat(v.to_bits());
                }
            }
            None => eat(u32::MAX),
        }
    }
    h
}

// ---------------------------------------------------------------------------------------------
// ECS side
// ---------------------------------------------------------------------------------------------

/// The frame the debris poses are computed for. With `auto` the [`DebrisPlugin`] advances it by
/// one each `Update`; a host with its own frame number (a sequence player, a render-out loop)
/// clears `auto` and writes `frame` itself, so debris is a pure function of that number.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebrisClock {
    /// The current frame.
    pub frame: u64,
    /// Advance by one per `Update`.
    pub auto: bool,
}

impl Default for DebrisClock {
    fn default() -> Self {
        Self { frame: 0, auto: true }
    }
}

/// One mesh variant a host registers for a class (typically loaded from a glTF).
#[derive(Clone, Debug)]
pub struct DebrisMesh {
    /// The mesh, authored about 1 m across (the fragment size scales it).
    pub mesh: Arc<CpuMesh>,
    /// The material.
    pub material: Arc<CpuMaterial>,
    /// Material used while the fragment is molten (`None`: the plain material).
    pub hot_material: Option<Arc<CpuMaterial>>,
}

/// The meshes available to debris, per class. Hosts fill it from loaded glTF files; a class with
/// no mesh spawns no fragments.
#[derive(Resource, Debug, Clone, Default)]
pub struct DebrisMeshLibrary {
    sets: HashMap<DebrisClass, Vec<DebrisMesh>>,
}

impl DebrisMeshLibrary {
    /// Adds a mesh variant to `class`.
    pub fn add(&mut self, class: DebrisClass, mesh: DebrisMesh) {
        self.sets.entry(class).or_default().push(mesh);
    }

    /// The variants of `class`.
    #[must_use]
    pub fn variants(&self, class: DebrisClass) -> &[DebrisMesh] {
        self.sets.get(&class).map_or(&[], Vec::as_slice)
    }

    /// Variants per class (zero for a class with none).
    #[must_use]
    pub fn counts(&self) -> [(DebrisClass, usize); 4] {
        DebrisClass::ALL.map(|c| (c, self.variants(c).len()))
    }

    fn pick(&self, class: DebrisClass, slot: u32) -> Option<&DebrisMesh> {
        let set = self.variants(class);
        if set.is_empty() {
            None
        } else {
            set.get(slot as usize % set.len())
        }
    }
}

/// Marks a fragment entity.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebrisFragment {
    /// The burst id.
    pub burst: u64,
    /// Index into the burst's fragment list.
    pub index: u32,
}

struct Burst {
    id: u64,
    spec: DebrisSpec,
    fragments: Vec<Fragment>,
    entities: Vec<Option<Entity>>,
    hot: Vec<bool>,
    /// Last `Transform` written for each fragment. A settled pose matches it, so the next
    /// frame does not mark the component changed.
    last_written: Vec<Option<Transform>>,
    removed: bool,
}

/// Why a burst could not be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebrisError {
    /// [`MAX_BURSTS`] bursts are alive.
    TooManyBursts,
    /// The spec holds no fragment.
    Empty,
}

impl std::fmt::Display for DebrisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyBursts => write!(f, "{MAX_BURSTS} debris bursts are already alive"),
            Self::Empty => write!(f, "the debris spec holds no fragments"),
        }
    }
}

impl std::error::Error for DebrisError {}

/// What `vfx.debris_state` reports about one burst.
#[derive(Clone, Debug, PartialEq)]
pub struct BurstInfo {
    /// Burst id.
    pub id: u64,
    /// Start frame.
    pub start_frame: u64,
    /// Fragments built.
    pub fragments: u32,
    /// Fragment entities alive now.
    pub live: u32,
    /// Fragments that have landed at the clock's frame.
    pub landed: u32,
    /// Hash of every pose at the clock's frame ([`poses_hash`]).
    pub hash: u64,
}

/// The live bursts.
#[derive(Resource, Default)]
pub struct DebrisBursts {
    bursts: Vec<Burst>,
    next_id: u64,
}

impl DebrisBursts {
    /// Moves every burst by `-shift` (spec 78 § 12.2): its origin and each fragment's launch,
    /// landing and rest points. Fragment poses are computed from these each frame, so the
    /// fragments' transforms follow.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for burst in &mut self.bursts {
            burst.spec.origin -= shift;
            for fragment in &mut burst.fragments {
                fragment.p0 -= shift;
                fragment.land_pos -= shift;
                fragment.rest_pos -= shift;
            }
        }
    }

    /// Creates a burst from `spec`, finding landings against `ground`. Returns its id.
    ///
    /// # Errors
    ///
    /// [`DebrisError`] when too many bursts are alive or the spec holds no fragment.
    pub fn spawn(&mut self, spec: DebrisSpec, ground: &dyn Fn(f32, f32) -> f32) -> Result<u64, DebrisError> {
        self.bursts.retain(|b| !b.removed);
        if self.bursts.len() >= MAX_BURSTS {
            return Err(DebrisError::TooManyBursts);
        }
        let fragments = build_fragments(&spec, ground);
        if fragments.is_empty() {
            return Err(DebrisError::Empty);
        }
        let id = self.next_id;
        self.next_id += 1;
        let n = fragments.len();
        self.bursts.push(Burst {
            id,
            spec,
            fragments,
            entities: vec![None; n],
            hot: vec![false; n],
            last_written: vec![None; n],
            removed: false,
        });
        Ok(id)
    }

    /// Marks a burst for removal; its entities despawn on the next `Update`. False when the id
    /// is unknown.
    pub fn remove(&mut self, id: u64) -> bool {
        match self.bursts.iter_mut().find(|b| b.id == id && !b.removed) {
            Some(b) => {
                b.removed = true;
                true
            }
            None => false,
        }
    }

    /// Marks every burst for removal.
    pub fn clear(&mut self) {
        for b in &mut self.bursts {
            b.removed = true;
        }
    }

    /// Live bursts (not marked removed).
    #[must_use]
    pub fn len(&self) -> usize {
        self.bursts.iter().filter(|b| !b.removed).count()
    }

    /// True when no burst is alive.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The fragments of burst `id`.
    #[must_use]
    pub fn fragments(&self, id: u64) -> Option<&[Fragment]> {
        self.bursts
            .iter()
            .find(|b| b.id == id && !b.removed)
            .map(|b| b.fragments.as_slice())
    }

    /// Summaries of the live bursts at `frame`.
    #[must_use]
    pub fn info(&self, frame: u64) -> Vec<BurstInfo> {
        self.bursts
            .iter()
            .filter(|b| !b.removed)
            .map(|b| {
                let t = burst_time(frame, b.spec.start_frame, b.spec.fps);
                let landed = b
                    .fragments
                    .iter()
                    .filter(|f| f.pose(t).is_some_and(|p| p.landed))
                    .count();
                BurstInfo {
                    id: b.id,
                    start_frame: b.spec.start_frame,
                    fragments: u32::try_from(b.fragments.len()).unwrap_or(u32::MAX),
                    live: u32::try_from(b.entities.iter().flatten().count()).unwrap_or(u32::MAX),
                    landed: u32::try_from(landed).unwrap_or(u32::MAX),
                    hash: poses_hash(&b.fragments, b.spec.start_frame, b.spec.fps, frame),
                }
            })
            .collect()
    }
}

/// A paused app holds the clock: its idle frames depend on how fast its clients talk to it
/// (ADR-2633).
pub(crate) fn tick_debris_clock(time: Option<Res<regia_app::Time>>, mut clock: ResMut<DebrisClock>) {
    if clock.auto && !time.is_some_and(|time| time.paused) {
        clock.frame += 1;
    }
}

/// Spawns, moves, swaps the material of and despawns the fragment entities for the clock's
/// frame. Each fragment's entity exists exactly when its pose does, so seeking the clock in
/// either direction gives the same scene as playing to that frame.
fn drive_debris(world: &mut World) {
    let _span = tracing::info_span!("debris_drive").entered();
    let frame = world.resource::<DebrisClock>().frame;
    world.resource_scope(|world, mut bursts: Mut<DebrisBursts>| {
        world.resource_scope(|world, library: Mut<DebrisMeshLibrary>| {
            for burst in &mut bursts.bursts {
                drive_burst(world, &library, burst, frame);
            }
        });
        bursts.bursts.retain(|b| !b.removed);
    });
}

fn drive_burst(world: &mut World, library: &DebrisMeshLibrary, burst: &mut Burst, frame: u64) {
    let t = burst_time(frame, burst.spec.start_frame, burst.spec.fps);
    for (i, fragment) in burst.fragments.iter().enumerate() {
        let pose = if burst.removed { None } else { fragment.pose(t) };
        match (pose, burst.entities[i]) {
            (Some(pose), None) => {
                let Some(mesh) = library.pick(fragment.class, fragment.mesh_slot) else {
                    continue;
                };
                let hot = pose.heat > 0.05 && mesh.hot_material.is_some();
                let material = match (&mesh.hot_material, hot) {
                    (Some(h), true) => Arc::clone(h),
                    _ => Arc::clone(&mesh.material),
                };
                let transform = Transform {
                    translation: pose.translation,
                    rotation: pose.rotation,
                    scale: Vec3::splat(pose.scale),
                };
                let entity = world
                    .spawn((
                        transform,
                        MeshRenderer::new(Arc::clone(&mesh.mesh), material),
                        DebrisFragment {
                            burst: burst.id,
                            index: u32::try_from(i).unwrap_or(u32::MAX),
                        },
                    ))
                    .id();
                burst.entities[i] = Some(entity);
                burst.hot[i] = hot;
                burst.last_written[i] = Some(transform);
            }
            (Some(pose), Some(entity)) => {
                let Ok(mut entry) = world.get_entity_mut(entity) else {
                    burst.entities[i] = None;
                    burst.last_written[i] = None;
                    continue;
                };
                let next = Transform {
                    translation: pose.translation,
                    rotation: pose.rotation,
                    scale: Vec3::splat(pose.scale),
                };
                if burst.last_written[i] != Some(next) {
                    if let Some(mut transform) = entry.get_mut::<Transform>() {
                        *transform = next;
                    }
                    burst.last_written[i] = Some(next);
                }
                let want_hot = pose.heat > 0.05;
                if want_hot != burst.hot[i]
                    && let Some(mesh) = library.pick(fragment.class, fragment.mesh_slot)
                    && let Some(hot_material) = &mesh.hot_material
                    && let Some(mut renderer) = entry.get_mut::<MeshRenderer>()
                {
                    renderer.material = if want_hot {
                        Arc::clone(hot_material)
                    } else {
                        Arc::clone(&mesh.material)
                    };
                    burst.hot[i] = want_hot;
                }
            }
            (None, Some(entity)) => {
                let _ = world.despawn(entity);
                burst.entities[i] = None;
                burst.hot[i] = false;
                burst.last_written[i] = None;
            }
            (None, None) => {}
        }
    }
}

/// Adds the debris resources and systems. Hosts register their meshes in
/// [`DebrisMeshLibrary`] and create bursts through [`DebrisBursts::spawn`] (or the
/// `vfx.debris_burst` agent method).
#[derive(Debug, Default, Clone, Copy)]
pub struct DebrisPlugin;

impl Plugin for DebrisPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DebrisClock>()
            .init_resource::<DebrisMeshLibrary>()
            .init_resource::<DebrisBursts>()
            .add_systems(Update, (tick_debris_clock, drive_debris).chain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(_x: f32, _z: f32) -> f32 {
        0.0
    }

    /// Pinned `poses_hash` values of `same_seed_gives_the_same_fragments` (deterministic
    /// kernels; a platform libm drifts from them).
    const EARTH_BLAST_HASH_START: u64 = 12_295_998_273_074_551_218;
    /// Frame 200 composes the det axis-angle with the fragment's start rotation through glam's
    /// `Quat` product, whose lanes are grouped differently on SSE2 and NEON (ADR-5176), so it is
    /// one value per instruction set; the M5 matched the frame-30 hashes and measured this one
    /// (ADR-5860).
    #[cfg(not(target_arch = "aarch64"))]
    const EARTH_BLAST_HASH_FRAME_200: u64 = 10_670_705_732_147_975_734;
    #[cfg(target_arch = "aarch64")]
    const EARTH_BLAST_HASH_FRAME_200: u64 = 10_398_085_621_204_273_512;
    const EARTH_BLAST_SEED1_HASH_START: u64 = 8_747_865_244_346_406_529;

    fn spec() -> DebrisSpec {
        DebrisSpec::earth_blast(Vec3::new(10.0, 0.0, -5.0), 0x00fe_4417, 30)
    }

    #[test]
    fn closed_form_matches_the_parabola_without_drag() {
        let p0 = Vec3::new(1.0, 2.0, 3.0);
        let v0 = Vec3::new(4.0, 20.0, -6.0);
        let t = 2.5;
        let p = flight_position(p0, v0, 9.81, 0.0, t);
        let want = p0 + v0 * t + Vec3::new(0.0, -0.5 * 9.81 * t * t, 0.0);
        assert!((p - want).length() < 1e-4);
    }

    #[test]
    fn small_drag_approaches_the_parabola() {
        let p0 = Vec3::ZERO;
        let v0 = Vec3::new(30.0, 40.0, 0.0);
        let free = flight_position(p0, v0, 9.81, 0.0, 3.0);
        let dragged = flight_position(p0, v0, 9.81, 0.0002, 3.0);
        assert!((free - dragged).length() < 0.1, "{free:?} vs {dragged:?}");
    }

    #[test]
    fn drag_shortens_the_flight() {
        let v0 = Vec3::new(60.0, 30.0, 0.0);
        let free = flight_position(Vec3::ZERO, v0, 9.81, 0.0, 4.0);
        let dragged = flight_position(Vec3::ZERO, v0, 9.81, 0.05, 4.0);
        assert!(dragged.x < free.x);
    }

    #[test]
    fn burst_time_quantizes_past_2_pow_24_frames() {
        // Honest about the f32 result (fix-vfx1-determinism finding 8, fixrev-22-k): adjacent
        // frames stay distinct while the delta fits an f32, and quantize to the same value
        // beyond it, where the f64 division keeps the magnitude correct.
        let fps = 30.0;
        for &(a, b) in &[(1u64, 2u64), (1000, 1001), (16_777_215, 16_777_216)] {
            assert_ne!(burst_time(a, 0, fps), burst_time(b, 0, fps), "frames {a}/{b}");
        }
        let a = burst_time(20_000_000, 0, fps);
        let b = burst_time(20_000_001, 0, fps);
        assert_eq!(a, b, "past 2^24 frames the f32 time cannot split adjacent frames");
        assert!(
            (a - 666_666.7).abs() < 1.0,
            "the f64 division keeps the magnitude: {a}"
        );
        assert!(burst_time(0, 10, fps) < 0.0, "before the start is negative");
    }

    #[test]
    fn velocity_is_the_derivative_of_position() {
        let (p0, v0, g, k) = (Vec3::ZERO, Vec3::new(20.0, 35.0, 5.0), 9.81, 0.03);
        let t = 1.7;
        let h = 0.05;
        let numeric =
            (flight_position(p0, v0, g, k, t + h) - flight_position(p0, v0, g, k, t - h)) / (2.0 * h);
        let exact = flight_velocity(v0, g, k, t);
        assert!((numeric - exact).length() < 0.1, "{numeric:?} vs {exact:?}");
    }

    #[test]
    fn same_seed_gives_the_same_fragments() {
        let a = build_fragments(&spec(), &flat);
        let b = build_fragments(&spec(), &flat);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1236);
        let mut other = spec();
        other.seed ^= 1;
        assert_ne!(a, build_fragments(&other, &flat));
        // Pinned across machines: the arcs, landings, orientations and molten decay all build
        // on the deterministic det kernels, so these hashes are the same on glibc, the Windows
        // CRT and musl (frame 200 is one value per instruction set, see its constant). Platform libm (sin/cos, exp, glam's quat paths) drifts from them
        // (fix-vfx1-determinism finding 3). Frames are at/after the start frame (30): earlier
        // every pose is `None` and the hash only counts the fragments.
        let s = spec();
        assert_eq!(poses_hash(&a, s.start_frame, s.fps, 30), EARTH_BLAST_HASH_START);
        assert_eq!(
            poses_hash(&a, s.start_frame, s.fps, 200),
            EARTH_BLAST_HASH_FRAME_200
        );
        // The audit's seed-1 blast, pinned the same way.
        let audit = DebrisSpec::earth_blast(Vec3::ZERO, 1, 30);
        let audit_frags = build_fragments(&audit, &flat);
        assert_eq!(
            poses_hash(&audit_frags, audit.start_frame, audit.fps, 30),
            EARTH_BLAST_SEED1_HASH_START
        );
    }

    #[test]
    fn poses_are_a_pure_function_of_the_frame() {
        let s = spec();
        let f = build_fragments(&s, &flat);
        let h1 = poses_hash(&f, s.start_frame, s.fps, 200);
        // Evaluating other frames in between changes nothing.
        let _ = poses_hash(&f, s.start_frame, s.fps, 5);
        let _ = poses_hash(&f, s.start_frame, s.fps, 900);
        assert_eq!(h1, poses_hash(&f, s.start_frame, s.fps, 200));
        assert_ne!(h1, poses_hash(&f, s.start_frame, s.fps, 201));
    }

    #[test]
    fn nothing_before_the_start_frame() {
        let s = spec();
        let f = build_fragments(&s, &flat);
        let t = burst_time(29, s.start_frame, s.fps);
        assert!(t < 0.0);
        assert!(f.iter().all(|fr| fr.pose(t).is_none()));
        assert!(f.iter().all(|fr| fr.pose(0.0).is_some()));
    }

    #[test]
    fn fragments_land_on_flat_ground_and_rest_partly_sunk() {
        let f = build_fragments(&spec(), &flat);
        for fr in &f {
            assert!(fr.land_t > 0.0 && fr.land_t < MAX_FLIGHT_S, "land {}", fr.land_t);
            let rest = fr.pose(fr.land_t + 60.0).expect("alive");
            assert!(rest.landed);
            // Resting centre height is the burial rule: size/2 * (1 - 2 burial).
            let burial = match fr.class {
                DebrisClass::Earth => 0.35,
                DebrisClass::Stone => 0.25,
                DebrisClass::Turf => 0.4,
                DebrisClass::Hardware => 0.2,
            };
            let want = fr.size * 0.5 * (1.0 - 2.0 * burial);
            assert!(
                (rest.translation.y - want).abs() < 1e-3,
                "{} vs {want}",
                rest.translation.y
            );
        }
    }

    #[test]
    fn landing_follows_sloped_ground() {
        let slope = |x: f32, _z: f32| 0.1 * x;
        let f = build_fragments(&spec(), &slope);
        for fr in &f {
            let p = fr.pose(fr.land_t).expect("alive");
            let ground = 0.1 * p.translation.x;
            // Touch-down centre is a rest offset above the ground there.
            assert!(
                p.translation.y >= ground - 1e-3,
                "{} below {ground}",
                p.translation.y
            );
            assert!(
                p.translation.y - ground <= fr.size,
                "floating {}",
                p.translation.y - ground
            );
        }
    }

    #[test]
    fn slide_is_capped_and_ends_at_the_rest_position() {
        let f = build_fragments(&spec(), &flat);
        for fr in &f {
            assert!(fr.slide_len <= MAX_SLIDE_M + 1e-4);
            let end = fr.pose(fr.land_t + 120.0).expect("alive");
            assert!((end.translation - fr.rest_pos).length() < 1e-3);
        }
    }

    #[test]
    fn ejecta_reaches_the_x2_envelope() {
        let f = build_fragments(&spec(), &flat);
        let apex = f
            .iter()
            .map(|fr| {
                // Apex time from the exact vertical velocity root, then the height there.
                let mut best = 0.0f32;
                let mut t = 0.0;
                while t < fr.land_t {
                    best = best.max(flight_position(fr.p0, fr.v0, fr.gravity, fr.drag, t).y);
                    t += 0.1;
                }
                best
            })
            .fold(0.0f32, f32::max);
        let range = f
            .iter()
            .map(|fr| (fr.land_pos - fr.p0).with_y(0.0).length())
            .fold(0.0f32, f32::max);
        assert!((150.0..=600.0).contains(&apex), "apex {apex}");
        assert!((300.0..=1100.0).contains(&range), "range {range}");
    }

    #[test]
    fn lifetime_ends_the_pose_and_shrinks_before_it() {
        let f = build_fragments(&spec(), &flat);
        let earth = f.iter().find(|fr| fr.class == DebrisClass::Earth).expect("earth");
        assert!(earth.pose(earth.lifetime_s - 0.001).expect("alive").scale < earth.size * 0.01);
        assert!(earth.pose(earth.lifetime_s).is_none());
        assert!((earth.pose(earth.land_t + 5.0).expect("alive").scale - earth.size).abs() < 1e-5);
        let plate = f
            .iter()
            .find(|fr| fr.class == DebrisClass::Hardware)
            .expect("hardware");
        assert!(plate.pose(1.0e6).is_some(), "hardware stays");
    }

    #[test]
    fn only_molten_fragments_glow_and_only_briefly() {
        let f = build_fragments(&spec(), &flat);
        assert!(f.iter().any(|fr| fr.molten));
        assert!(
            f.iter()
                .filter(|fr| fr.molten)
                .all(|fr| fr.class == DebrisClass::Hardware)
        );
        for fr in &f {
            let early = fr.pose(0.05).expect("alive").heat;
            if fr.molten {
                assert!(early > 0.9);
                assert!(fr.pose(1.5).expect("alive").heat < early);
                assert_eq!(fr.pose(MOLTEN_MAX_S + 0.1).expect("alive").heat, 0.0);
            } else {
                assert_eq!(early, 0.0);
            }
        }
    }

    #[test]
    fn cheap_detail_has_no_glow_and_no_fade() {
        let mut s = spec();
        s.apply_tier(QualityTier::Low);
        let f = build_fragments(&s, &flat);
        assert!(f.iter().all(|fr| !fr.molten));
        let earth = f.iter().find(|fr| fr.class == DebrisClass::Earth).expect("earth");
        assert!((earth.pose(earth.lifetime_s - 0.001).expect("alive").scale - earth.size).abs() < 1e-5);
    }

    #[test]
    fn budget_caps_the_count_and_keeps_hardware_first() {
        for tier in QualityTier::ALL {
            let mut s = spec();
            s.apply_tier(tier);
            assert!(s.total() <= fragment_budget(tier), "{tier:?} {}", s.total());
            let plates = s
                .classes
                .iter()
                .find(|c| c.class == DebrisClass::Hardware)
                .map_or(0, |c| c.count);
            assert!(plates > 0, "{tier:?} keeps some hardware");
        }
        let mut ultra = spec();
        ultra.apply_tier(QualityTier::Ultra);
        assert_eq!(ultra.total(), 1236, "Ultra keeps the whole burst");
    }

    /// Hardware keeps at most a quarter of the cap. Other classes share the rest and can be
    /// raised. An all-hardware burst stops at the quarter.
    #[test]
    fn budget_keeps_at_most_a_quarter_hardware_and_can_raise_another_class() {
        let count = |spec: &DebrisSpec, class: DebrisClass| {
            spec.classes
                .iter()
                .find(|c| c.class == class)
                .map_or(0, |c| c.count)
        };
        let mut earth = spec();
        earth.apply_budget(48);
        assert_eq!(count(&earth, DebrisClass::Hardware), 12);
        assert_eq!(count(&earth, DebrisClass::Earth), 15);
        assert_eq!(count(&earth, DebrisClass::Stone), 11);
        assert_eq!(count(&earth, DebrisClass::Turf), 9);
        assert_eq!(earth.total(), 47);

        let mut raised = spec();
        raised.classes = vec![DebrisClassSpec::hardware(100), DebrisClassSpec::earth(10)];
        raised.apply_budget(48);
        assert_eq!(count(&raised, DebrisClass::Hardware), 12);
        assert_eq!(count(&raised, DebrisClass::Earth), 36);

        let mut plates = spec();
        plates.classes = vec![DebrisClassSpec::hardware(200)];
        plates.apply_budget(48);
        assert_eq!(plates.total(), 12);
    }

    /// Class counts that add up past `u32::MAX` are capped like any other burst (corr-vfx L3):
    /// the hardware sum used to overflow a u32 (a panic with overflow checks, a wrap without).
    #[test]
    fn budget_sums_counts_past_u32_max() {
        let count = |spec: &DebrisSpec, class: DebrisClass| -> u32 {
            spec.classes
                .iter()
                .filter(|c| c.class == class)
                .map(|c| c.count)
                .sum()
        };
        let half = u32::MAX / 2 + 1;
        let mut huge = spec();
        huge.classes = vec![
            DebrisClassSpec::hardware(half),
            DebrisClassSpec::hardware(half),
            DebrisClassSpec::earth(10),
        ];
        huge.apply_budget(48);
        assert_eq!(count(&huge, DebrisClass::Hardware), 12);
        assert_eq!(count(&huge, DebrisClass::Earth), 36);

        let mut others = spec();
        others.classes = vec![
            DebrisClassSpec::earth(half),
            DebrisClassSpec::stone(half),
            DebrisClassSpec::hardware(4),
        ];
        others.apply_budget(48);
        assert_eq!(count(&others, DebrisClass::Hardware), 4);
        assert_eq!(count(&others, DebrisClass::Earth), 22);
        assert_eq!(count(&others, DebrisClass::Stone), 22);
    }

    /// Touch-down keeps 0.35 of the horizontal speed. The slide is that speed's Coulomb distance.
    #[test]
    fn slide_uses_the_retained_landing_speed() {
        let mut s = spec();
        s.origin = Vec3::new(0.0, 0.5, 0.0);
        s.elevation = [0.0, 0.0];
        s.gravity = 9.81;
        s.friction = 0.6;
        s.classes = vec![DebrisClassSpec {
            speed: [5.0, 5.0],
            size: [1.0, 1.0],
            drag: 0.0,
            burial: 0.0,
            count: 1,
            ..DebrisClassSpec::earth(1)
        }];
        let fr = &build_fragments(&s, &flat)[0];
        let landed = flight_velocity(fr.v0, fr.gravity, fr.drag, fr.land_t);
        let horizontal = Vec3::new(landed.x, 0.0, landed.z).length();
        assert!((horizontal - 5.0).abs() < 0.05, "{horizontal}");
        let mu = s.friction.max(0.05);
        let expect = (0.35 * horizontal).powi(2) / (2.0 * mu * s.gravity);
        assert!(
            (fr.slide_len - expect).abs() < 1e-3,
            "{} vs {expect}",
            fr.slide_len
        );
        assert!(fr.slide_len < 1.0, "not the full-speed Coulomb distance");
    }

    #[test]
    fn a_tier_cap_keeps_the_arcs_of_the_fragments_that_remain() {
        let full = build_fragments(&spec(), &flat);
        let mut capped = spec();
        capped.apply_budget(300);
        let small = build_fragments(&capped, &flat);
        let first_earth = small
            .iter()
            .find(|fr| fr.class == DebrisClass::Earth && fr.index == 3)
            .expect("kept");
        let same = full
            .iter()
            .find(|fr| fr.class == DebrisClass::Earth && fr.index == 3)
            .expect("full");
        assert_eq!(first_earth, same);
    }

    #[test]
    fn class_names_round_trip() {
        for c in DebrisClass::ALL {
            assert_eq!(DebrisClass::from_name(c.name()), Some(c));
        }
        assert_eq!(DebrisClass::from_name("glass"), None);
    }

    #[test]
    fn bursts_are_capped_and_empty_specs_refused() {
        let mut bursts = DebrisBursts::default();
        let mut empty = spec();
        empty.classes.clear();
        assert_eq!(bursts.spawn(empty, &flat), Err(DebrisError::Empty));
        for _ in 0..MAX_BURSTS {
            let mut s = spec();
            s.apply_budget(8);
            bursts.spawn(s, &flat).expect("room");
        }
        let mut s = spec();
        s.apply_budget(8);
        assert_eq!(bursts.spawn(s, &flat), Err(DebrisError::TooManyBursts));
        assert!(bursts.remove(0));
        assert!(!bursts.remove(0));
        assert_eq!(bursts.len(), MAX_BURSTS - 1);
    }

    fn library() -> DebrisMeshLibrary {
        let mut lib = DebrisMeshLibrary::default();
        for class in DebrisClass::ALL {
            lib.add(
                class,
                DebrisMesh {
                    mesh: Arc::new(CpuMesh {
                        vertices: vec![],
                        indices: vec![],
                    }),
                    material: Arc::new(CpuMaterial::default()),
                    hot_material: None,
                },
            );
        }
        lib
    }

    fn app_with_burst(frames_to_run: u64) -> (App, u64) {
        let mut app = App::new();
        app.add_plugins(DebrisPlugin);
        *app.world_mut().resource_mut::<DebrisMeshLibrary>() = library();
        let mut s = spec();
        s.apply_budget(64);
        s.start_frame = 2;
        let id = app
            .world_mut()
            .resource_mut::<DebrisBursts>()
            .spawn(s, &flat)
            .expect("burst");
        for _ in 0..frames_to_run {
            app.update();
        }
        (app, id)
    }

    fn fragment_count(app: &mut App) -> usize {
        app.world_mut()
            .query::<&DebrisFragment>()
            .iter(app.world())
            .count()
    }

    #[test]
    fn entities_exist_exactly_when_their_pose_does() {
        let (mut app, _) = app_with_burst(1);
        assert_eq!(fragment_count(&mut app), 0, "before the start frame");
        for _ in 0..3 {
            app.update();
        }
        let live = fragment_count(&mut app);
        let built = app.world().resource::<DebrisBursts>().info(4)[0].fragments as usize;
        assert!(live > 40 && live == built, "{live} of {built}");
        // Seek far past every finite lifetime: only the plates remain.
        app.world_mut().resource_mut::<DebrisClock>().auto = false;
        app.world_mut().resource_mut::<DebrisClock>().frame = 2 + 60 * 400;
        app.update();
        let left = fragment_count(&mut app);
        assert!(left > 0 && left < live, "{left} of {live}");
        // Seek back: the same scene as playing to that frame.
        app.world_mut().resource_mut::<DebrisClock>().frame = 10;
        app.update();
        assert_eq!(fragment_count(&mut app), live);
        // Before the start: all gone.
        app.world_mut().resource_mut::<DebrisClock>().frame = 0;
        app.update();
        assert_eq!(fragment_count(&mut app), 0);
    }

    #[test]
    fn removing_a_burst_despawns_its_fragments() {
        let (mut app, id) = app_with_burst(5);
        assert!(fragment_count(&mut app) > 0);
        assert!(app.world_mut().resource_mut::<DebrisBursts>().remove(id));
        app.update();
        assert_eq!(fragment_count(&mut app), 0);
        assert!(app.world().resource::<DebrisBursts>().is_empty());
    }

    #[test]
    fn a_class_without_a_mesh_spawns_nothing() {
        let mut app = App::new();
        app.add_plugins(DebrisPlugin);
        let mut s = spec();
        s.apply_budget(40);
        s.start_frame = 0;
        app.world_mut()
            .resource_mut::<DebrisBursts>()
            .spawn(s, &flat)
            .expect("burst");
        app.update();
        app.update();
        assert_eq!(fragment_count(&mut app), 0);
    }

    /// Once every fragment has landed and settled, the next frame leaves its `Transform`
    /// change tick alone.
    #[test]
    fn settled_fragments_do_not_rewrite_transform() {
        let mut app = App::new();
        app.add_plugins(DebrisPlugin);
        *app.world_mut().resource_mut::<DebrisMeshLibrary>() = library();
        let mut spec = spec();
        spec.apply_budget(4);
        spec.start_frame = 2;
        let fragments = build_fragments(&spec, &flat);
        assert!(!fragments.is_empty());
        let start = spec.start_frame;
        let fps = spec.fps;
        let ready = fragments.iter().fold(0.0f32, |acc, fragment| {
            let slide = fragment.slide_speed / fragment.slide_decel;
            acc.max(fragment.land_t + slide.max(fragment.settle_s))
        });
        let frame = start + (f64::from(ready) * f64::from(fps)).ceil() as u64 + 2;
        app.world_mut()
            .resource_mut::<DebrisBursts>()
            .spawn(spec, &flat)
            .expect("burst");
        {
            let mut clock = app.world_mut().resource_mut::<DebrisClock>();
            clock.auto = false;
            clock.frame = frame;
        }
        app.update();
        let t0 = burst_time(frame, start, fps);
        let t1 = burst_time(frame + 1, start, fps);
        for fragment in &fragments {
            let a = fragment.pose(t0).expect("alive");
            let b = fragment.pose(t1).expect("still alive");
            assert_eq!(
                (a.translation, a.rotation, a.scale),
                (b.translation, b.rotation, b.scale),
                "fragment {} is still moving at t={t0}",
                fragment.index
            );
        }
        let rows: Vec<_> = app
            .world_mut()
            .query::<(Entity, &DebrisFragment, &Transform)>()
            .iter(app.world())
            .map(|(entity, fragment, transform)| {
                (
                    entity,
                    fragment.index,
                    transform.translation,
                    transform.rotation,
                    transform.scale,
                )
            })
            .collect();
        assert_eq!(rows.len(), fragments.len(), "every fragment has an entity");
        let ticks: Vec<_> = rows
            .iter()
            .map(|(entity, _, _, _, _)| {
                app.world()
                    .get_entity(*entity)
                    .ok()
                    .and_then(|entry| entry.get_change_ticks::<Transform>())
                    .map(|ticks| ticks.changed)
            })
            .collect();
        app.world_mut().resource_mut::<DebrisClock>().frame = frame + 1;
        app.update();
        for ((entity, index, translation, rotation, scale), tick) in rows.iter().zip(&ticks) {
            let transform = app
                .world()
                .get_entity(*entity)
                .ok()
                .and_then(|entry| entry.get::<Transform>().copied())
                .expect("transform");
            assert_eq!(transform.translation, *translation, "fragment {index} moved");
            assert_eq!(transform.rotation, *rotation);
            assert_eq!(transform.scale, *scale);
            let now = app
                .world()
                .get_entity(*entity)
                .ok()
                .and_then(|entry| entry.get_change_ticks::<Transform>())
                .map(|ticks| ticks.changed);
            assert_eq!(
                now, *tick,
                "fragment {index} rewrote Transform while its pose was unchanged"
            );
        }
    }
}
