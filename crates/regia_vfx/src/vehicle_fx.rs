// SPDX-License-Identifier: MPL-2.0
//! Wheel surface effects: the smoke, dust and spray a wheel kicks up and the marks it leaves.
//!
//! Pure logic, no ECS and no GPU. A host (`regia_framework::vehicle_fx`) samples each wheel once per
//! fixed step ([`WheelSample`]) and [`VehicleFx::step`] answers with what to play ([`WheelOutput`]:
//! intensities and spawn rates for the three built-in effects [`TIRE_SMOKE`], [`WHEEL_DUST`] and
//! [`WHEEL_SPRAY`]) and which decal stamps to lay or retire.
//!
//! - **Slip.** The wheel's contact velocity is differenced from its contact point. With the wheel's
//!   heading it splits into a longitudinal part `v_long` and a lateral part `v_lat`; with the
//!   wheel's rolling speed `ω·r` the combined slip is `hypot((ω·r − v_long) / max(|v_long|, 1),
//!   v_lat / max(|v_long|, 1))`, clamped to 2 (the ratio of the brush-tyre model's longitudinal
//!   slip and the tangent of the slip angle).
//! - **Smoke** on hard surfaces ([`WheelSurface::Asphalt`], [`WheelSurface::Concrete`]): above
//!   [`VehicleFxConfig::smoke_slip_threshold`], with density `(slip - threshold) / (1 - threshold) × clamp(speed / 20, 0.2, 1) × effect_scale`.
//! - **Dust** on loose surfaces (gravel, dirt, sand, grass, snow): rises with speed and slip.
//! - **Spray** on water and mud, by speed.
//! - **Marks.** A skid strip on a hard surface while the slip is over
//!   [`VehicleFxConfig::skid_slip_threshold`] (it ends below 80 % of it), a rut on a soft one while
//!   the wheel rolls. A strip is a run of overlapping projected stamps laid out exactly as
//!   [`crate::decal_strip::DecalStrip`] lays them (same spacing, carry between segments, stamp
//!   length and orientation; a parity test compares them). The polyline gets a new vertex when the
//!   contact moved at least `spacing`, or turned more than [`VehicleFxConfig::max_turn`] over at least
//!   a quarter of it.
//! - **The pool.** [`VehicleFxLimits::max_stamps`] caps the stamps alive across every wheel; the
//!   oldest retires first. A stamp also retires after [`VehicleFxConfig::mark_lifetime_s`] (its
//!   decal fades out over the last 30 % of that, see [`crate::decal::Decal::fade`]).
//!
//! Everything advances by the `dt` handed in: no clock, no randomness, no hash iteration, so two
//! runs on the same samples agree bit for bit ([`VehicleFx::state_digest`]). After the warm-up (the
//! first wheel and the first full pool) a step does not allocate.
//!
//! The effects are Rust definitions like the hover washes ([`crate::hover`]); [`vehicle_effect`]
//! builds them and [`ensure_vehicle_effect`] registers one in a [`crate::scene::VfxScene`].

use std::collections::VecDeque;

use regia_core::math::Vec3;
use regia_render::quality::QualityTier;

use crate::atmos_presets::{keys, r, ramp};
use crate::decal::{Decal, DecalSurface, align_along};
use crate::format::{
    BlendMode, EmitterDef, FlipbookDef, FlipbookMode, InitModule, LodDef, RenderMode, RenderModule, ShapeDef,
    SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef,
    VelocityMode, VfxSystemDef,
};

/// The built-in tyre smoke effect (hard surfaces, over the slip threshold).
pub const TIRE_SMOKE: &str = "tire_smoke";
/// The built-in dust effect (loose surfaces, by speed).
pub const WHEEL_DUST: &str = "wheel_dust";
/// The built-in spray effect (water, and mud thrown up, by speed).
pub const WHEEL_SPRAY: &str = "wheel_spray";

/// Spawn rate of [`TIRE_SMOKE`] at intensity 1, particles per second.
pub const SMOKE_RATE_MAX: f32 = 120.0;
/// Spawn rate of [`WHEEL_DUST`] at intensity 1, particles per second.
pub const DUST_RATE_MAX: f32 = 160.0;
/// Spawn rate of [`WHEEL_SPRAY`] at intensity 1, particles per second.
pub const SPRAY_RATE_MAX: f32 = 200.0;
/// The most wheels one [`VehicleFx`] tracks; further keys are ignored.
pub const MAX_FX_WHEELS: usize = 64;
/// The largest stamp cap any tier or caller may ask for.
pub const MAX_STAMP_CAP: usize = 8192;

/// What a wheel rolls on, as far as the effects are concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum WheelSurface {
    /// Road asphalt: smoke and skid marks.
    #[default]
    Asphalt,
    /// Concrete or paving: smoke and skid marks.
    Concrete,
    /// Gravel or crushed stone: dust and ruts.
    Gravel,
    /// Packed earth: dust and ruts.
    Dirt,
    /// Sand: a lot of dust and deep ruts.
    Sand,
    /// Mud: spray and deep ruts.
    Mud,
    /// Grass: a little dust, light ruts.
    Grass,
    /// Snow: powder and ruts.
    Snow,
    /// Standing water: spray, no mark.
    Water,
}

/// What kind of mark a surface takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum MarkKind {
    /// No mark this step.
    #[default]
    None,
    /// A dark rubber streak (hard surface, slip over the threshold).
    Skid,
    /// A tyre rut (soft surface, wheel rolling).
    Rut,
}

impl MarkKind {
    /// Lower-case name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Skid => "skid",
            Self::Rut => "rut",
        }
    }
}

/// How one surface behaves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceFx {
    /// The mark the surface takes.
    pub mark: MarkKind,
    /// Tyre smoke under slip.
    pub smoke: bool,
    /// Dust particles per second per m/s of speed (0 = none).
    pub dust_gain: f32,
    /// Spray particles per second per m/s of speed (0 = none).
    pub spray_gain: f32,
    /// Linear mark colour.
    pub mark_color: [f32; 3],
    /// Peak mark opacity.
    pub mark_opacity: f32,
    /// Mark half-width relative to the wheel's half-width.
    pub mark_width_scale: f32,
}

impl WheelSurface {
    /// Every surface, in a fixed order.
    pub const ALL: [Self; 9] = [
        Self::Asphalt,
        Self::Concrete,
        Self::Gravel,
        Self::Dirt,
        Self::Sand,
        Self::Mud,
        Self::Grass,
        Self::Snow,
        Self::Water,
    ];

    /// Lower-case name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Asphalt => "asphalt",
            Self::Concrete => "concrete",
            Self::Gravel => "gravel",
            Self::Dirt => "dirt",
            Self::Sand => "sand",
            Self::Mud => "mud",
            Self::Grass => "grass",
            Self::Snow => "snow",
            Self::Water => "water",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The surface a physics-material or terrain-layer name suggests: the first surface whose
    /// keyword the (case-folded) name contains. `None` when no keyword matches.
    #[must_use]
    pub fn from_material_name(name: &str) -> Option<Self> {
        const WORDS: [(&str, WheelSurface); 17] = [
            ("asphalt", WheelSurface::Asphalt),
            ("tarmac", WheelSurface::Asphalt),
            ("road", WheelSurface::Asphalt),
            ("concrete", WheelSurface::Concrete),
            ("pavement", WheelSurface::Concrete),
            ("gravel", WheelSurface::Gravel),
            ("pebble", WheelSurface::Gravel),
            ("rock", WheelSurface::Gravel),
            ("sand", WheelSurface::Sand),
            ("mud", WheelSurface::Mud),
            ("snow", WheelSurface::Snow),
            ("ice", WheelSurface::Snow),
            ("grass", WheelSurface::Grass),
            ("water", WheelSurface::Water),
            ("puddle", WheelSurface::Water),
            ("dirt", WheelSurface::Dirt),
            ("soil", WheelSurface::Dirt),
        ];
        let lower = name.to_ascii_lowercase();
        WORDS.iter().find(|(w, _)| lower.contains(w)).map(|&(_, s)| s)
    }

    /// The surface's effect numbers.
    #[must_use]
    pub fn fx(self) -> SurfaceFx {
        let base = SurfaceFx {
            mark: MarkKind::None,
            smoke: false,
            dust_gain: 0.0,
            spray_gain: 0.0,
            mark_color: [0.02, 0.02, 0.02],
            mark_opacity: 0.0,
            mark_width_scale: 1.0,
        };
        match self {
            Self::Asphalt => SurfaceFx {
                mark: MarkKind::Skid,
                smoke: true,
                mark_opacity: 0.85,
                mark_width_scale: 0.9,
                ..base
            },
            Self::Concrete => SurfaceFx {
                mark: MarkKind::Skid,
                smoke: true,
                mark_color: [0.04, 0.04, 0.04],
                mark_opacity: 0.7,
                mark_width_scale: 0.9,
                ..base
            },
            Self::Gravel => SurfaceFx {
                mark: MarkKind::Rut,
                dust_gain: 3.0,
                mark_color: [0.1, 0.09, 0.08],
                mark_opacity: 0.45,
                ..base
            },
            Self::Dirt => SurfaceFx {
                mark: MarkKind::Rut,
                dust_gain: 4.0,
                mark_color: [0.07, 0.05, 0.035],
                mark_opacity: 0.6,
                ..base
            },
            Self::Sand => SurfaceFx {
                mark: MarkKind::Rut,
                dust_gain: 5.0,
                mark_color: [0.3, 0.24, 0.15],
                mark_opacity: 0.5,
                mark_width_scale: 1.1,
                ..base
            },
            Self::Mud => SurfaceFx {
                mark: MarkKind::Rut,
                spray_gain: 3.0,
                mark_color: [0.03, 0.022, 0.015],
                mark_opacity: 0.8,
                mark_width_scale: 1.1,
                ..base
            },
            Self::Grass => SurfaceFx {
                mark: MarkKind::Rut,
                dust_gain: 0.8,
                mark_color: [0.04, 0.05, 0.02],
                mark_opacity: 0.3,
                ..base
            },
            Self::Snow => SurfaceFx {
                mark: MarkKind::Rut,
                dust_gain: 3.0,
                mark_color: [0.45, 0.47, 0.5],
                mark_opacity: 0.55,
                ..base
            },
            Self::Water => SurfaceFx {
                spray_gain: 6.0,
                ..base
            },
        }
    }
}

/// Stamp cap and effect density for a quality tier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VehicleFxLimits {
    /// Most stamps alive across every wheel (the oldest retires first).
    pub max_stamps: usize,
    /// Multiplies every effect intensity (and so its spawn rate).
    pub effect_scale: f32,
}

impl VehicleFxLimits {
    /// The limits of a tier: a fixed table, 24 stamps on the weakest phone up to 512 on Ultra.
    #[must_use]
    pub const fn for_tier(tier: QualityTier) -> Self {
        let (max_stamps, effect_scale) = match tier {
            QualityTier::MobileLow => (24, 0.4),
            QualityTier::MobileHigh => (32, 0.6),
            QualityTier::Deck => (48, 0.6),
            QualityTier::Low | QualityTier::VeryLow => (48, 0.5),
            QualityTier::Medium => (96, 0.75),
            QualityTier::High => (128, 1.0),
            QualityTier::Ultra => (512, 1.0),
        };
        Self {
            max_stamps,
            effect_scale,
        }
    }
}

impl Default for VehicleFxLimits {
    fn default() -> Self {
        Self::for_tier(QualityTier::High)
    }
}

/// Tuning of the wheel effects.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VehicleFxConfig {
    /// Distance between stamp centres, metres.
    pub spacing: f32,
    /// A turn of the contact path over this angle (radians) starts a new polyline vertex.
    pub max_turn: f32,
    /// Combined slip above which a hard surface smokes.
    pub smoke_slip_threshold: f32,
    /// Combined slip above which a hard surface takes a skid mark (it ends below 80 % of it).
    pub skid_slip_threshold: f32,
    /// Speed below which a wheel makes no rut, no dust and no spray, m/s.
    pub min_speed: f32,
    /// Seconds a stamp lives; its decal fades out over the last 30 %.
    pub mark_lifetime_s: f32,
    /// Projection half-depth of a stamp, metres.
    pub half_depth: f32,
    /// The tier limits.
    pub limits: VehicleFxLimits,
}

impl Default for VehicleFxConfig {
    fn default() -> Self {
        Self {
            spacing: 0.5,
            max_turn: 0.14,
            smoke_slip_threshold: 0.25,
            skid_slip_threshold: 0.3,
            min_speed: 0.8,
            mark_lifetime_s: 60.0,
            half_depth: 0.25,
            limits: VehicleFxLimits::default(),
        }
    }
}

/// One wheel at one fixed step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelSample {
    /// A key that is stable for the wheel (entity bits and wheel index).
    pub key: u64,
    /// The wheel touches the ground.
    pub grounded: bool,
    /// Where it touches, world space.
    pub contact: Vec3,
    /// The ground normal there.
    pub normal: Vec3,
    /// Where the wheel points, world space (any length; it is projected onto the ground).
    pub heading: Vec3,
    /// Rolling speed `ω·r` along the heading, m/s (positive rolling forward).
    pub roll_speed: f32,
    /// What the wheel rolls on.
    pub surface: WheelSurface,
    /// Half the tyre's width, metres.
    pub half_width: f32,
}

/// What one wheel should do this step.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct WheelOutput {
    /// The wheel's key.
    pub key: u64,
    /// Longitudinal slip ratio `(ω·r − v_long) / max(|v_long|, 1)`.
    pub slip_ratio: f32,
    /// Lateral slip `|v_lat| / max(|v_long|, 1)`.
    pub slip_lateral: f32,
    /// Combined slip, `0..=2`.
    pub slip: f32,
    /// Contact speed, m/s.
    pub speed: f32,
    /// [`TIRE_SMOKE`] intensity `0..=1` (tier scale applied).
    pub smoke: f32,
    /// [`WHEEL_DUST`] intensity `0..=1`.
    pub dust: f32,
    /// [`WHEEL_SPRAY`] intensity `0..=1`.
    pub spray: f32,
    /// Smoke particles per second ([`SMOKE_RATE_MAX`] × `smoke`).
    pub smoke_rate: f32,
    /// Dust particles per second.
    pub dust_rate: f32,
    /// Spray particles per second.
    pub spray_rate: f32,
    /// The mark being laid this step.
    pub mark: MarkKind,
    /// Stamps this wheel laid this step.
    pub new_stamps: u32,
}

/// A stamp laid this step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NewStamp {
    /// Unique, increasing serial. Retirements name it.
    pub serial: u64,
    /// The projected decal (lifetime set, age 0).
    pub decal: Decal,
    /// Its material.
    pub surface: DecalSurface,
}

/// How one stamp looks.
struct StampStyle {
    normal: Vec3,
    half_width: f32,
    along: f32,
    color: [f32; 3],
    opacity: f32,
}

#[derive(Clone, Copy, Debug)]
struct Run {
    last_vertex: Vec3,
    last_dir: Vec3,
    /// Distance from `last_vertex` to the next stamp centre.
    to_next: f32,
    kind: MarkKind,
}

#[derive(Clone, Debug)]
struct WheelFx {
    key: u64,
    prev_contact: Option<Vec3>,
    run: Option<Run>,
    skid_on: bool,
    last: WheelOutput,
    surface: WheelSurface,
}

/// Counters of the stamp pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct StampStats {
    /// Stamps alive now.
    pub live: usize,
    /// Stamps laid since the start.
    pub emitted: u64,
    /// Stamps retired since the start (cap, age or [`VehicleFx::clear`]).
    pub retired: u64,
    /// Of those, retired by the cap.
    pub retired_by_cap: u64,
}

/// The per-wheel state machine and the stamp pool for every wheel of a scene.
#[derive(Clone, Debug)]
pub struct VehicleFx {
    config: VehicleFxConfig,
    wheels: Vec<WheelFx>,
    /// `(serial, birth time)`, oldest first.
    ring: VecDeque<(u64, f32)>,
    clock: f32,
    next_serial: u64,
    stats: StampStats,
    fresh: Vec<NewStamp>,
    gone: Vec<u64>,
}

fn smooth_plane(v: Vec3, n: Vec3) -> Vec3 {
    v - n * v.dot(n)
}

impl VehicleFx {
    /// An empty set with `config` (the cap is clamped to [`MAX_STAMP_CAP`]).
    #[must_use]
    pub fn new(mut config: VehicleFxConfig) -> Self {
        config.limits.max_stamps = config.limits.max_stamps.min(MAX_STAMP_CAP);
        let cap = config.limits.max_stamps;
        Self {
            config,
            wheels: Vec::new(),
            ring: VecDeque::with_capacity(cap + 1),
            clock: 0.0,
            next_serial: 0,
            stats: StampStats::default(),
            fresh: Vec::with_capacity(64),
            gone: Vec::with_capacity(64),
        }
    }

    /// The tuning.
    #[must_use]
    pub fn config(&self) -> &VehicleFxConfig {
        &self.config
    }

    /// Swaps the tuning; a lower cap retires the oldest stamps at the next step.
    pub fn set_config(&mut self, mut config: VehicleFxConfig) {
        config.limits.max_stamps = config.limits.max_stamps.min(MAX_STAMP_CAP);
        let cap = config.limits.max_stamps;
        if self.ring.capacity() < cap + 1 {
            self.ring.reserve(cap + 1 - self.ring.len());
        }
        self.config = config;
    }

    /// Stamp counters.
    #[must_use]
    pub fn stats(&self) -> StampStats {
        StampStats {
            live: self.ring.len(),
            ..self.stats
        }
    }

    /// Wheels being tracked.
    #[must_use]
    pub fn wheel_count(&self) -> usize {
        self.wheels.len()
    }

    /// The last output of the wheel with `key`, if tracked.
    #[must_use]
    pub fn output_of(&self, key: u64) -> Option<(WheelOutput, WheelSurface)> {
        self.wheels
            .iter()
            .find(|w| w.key == key)
            .map(|w| (w.last, w.surface))
    }

    /// The last output of every tracked wheel, in first-seen order.
    pub fn outputs(&self) -> impl Iterator<Item = (WheelOutput, WheelSurface)> + '_ {
        self.wheels.iter().map(|w| (w.last, w.surface))
    }

    /// Stamps laid by the last [`Self::step`].
    #[must_use]
    pub fn new_stamps(&self) -> &[NewStamp] {
        &self.fresh
    }

    /// Serials retired by the last [`Self::step`] (oldest first).
    #[must_use]
    pub fn retired(&self) -> &[u64] {
        &self.gone
    }

    /// Retires every stamp (the caller removes the decals named by [`Self::retired`]) and forgets
    /// the wheels' runs; the counters keep counting.
    pub fn clear(&mut self) {
        self.gone.clear();
        self.fresh.clear();
        while let Some((serial, _)) = self.ring.pop_front() {
            self.gone.push(serial);
            self.stats.retired += 1;
        }
        for w in &mut self.wheels {
            w.run = None;
            w.skid_on = false;
            w.prev_contact = None;
        }
    }

    /// Advances every sampled wheel by `dt` seconds and fills `out` (cleared first) with one
    /// output per sample, in sample order. Wheels missing from `samples` keep their state.
    pub fn step(&mut self, dt: f32, samples: &[WheelSample], out: &mut Vec<WheelOutput>) {
        let _span = tracing::debug_span!("vehicle_fx_step", wheels = samples.len()).entered();
        out.clear();
        self.fresh.clear();
        self.gone.clear();
        let dt = if dt.is_finite() && dt > 1e-6 {
            dt
        } else {
            1.0 / 60.0
        };
        self.clock += dt;
        for sample in samples {
            let output = self.step_wheel(dt, sample);
            out.push(output);
        }
        self.expire();
    }

    fn expire(&mut self) {
        let life = self.config.mark_lifetime_s.max(0.1);
        while let Some(&(serial, born)) = self.ring.front() {
            if self.clock - born < life {
                break;
            }
            self.ring.pop_front();
            self.gone.push(serial);
            self.stats.retired += 1;
        }
        let cap = self.config.limits.max_stamps;
        while self.ring.len() > cap {
            if let Some((serial, _)) = self.ring.pop_front() {
                self.gone.push(serial);
                self.stats.retired += 1;
                self.stats.retired_by_cap += 1;
            }
        }
    }

    fn slot(&mut self, key: u64) -> Option<usize> {
        if let Some(i) = self.wheels.iter().position(|w| w.key == key) {
            return Some(i);
        }
        if self.wheels.len() >= MAX_FX_WHEELS {
            return None;
        }
        self.wheels.push(WheelFx {
            key,
            prev_contact: None,
            run: None,
            skid_on: false,
            last: WheelOutput {
                key,
                ..WheelOutput::default()
            },
            surface: WheelSurface::default(),
        });
        Some(self.wheels.len() - 1)
    }

    fn step_wheel(&mut self, dt: f32, s: &WheelSample) -> WheelOutput {
        let mut o = WheelOutput {
            key: s.key,
            ..WheelOutput::default()
        };
        let Some(i) = self.slot(s.key) else { return o };
        let finite = s.contact.is_finite()
            && s.normal.is_finite()
            && s.heading.is_finite()
            && s.roll_speed.is_finite();
        if !s.grounded || !finite {
            let w = &mut self.wheels[i];
            w.prev_contact = None;
            w.run = None;
            w.skid_on = false;
            w.surface = s.surface;
            w.last = o;
            return o;
        }
        let normal = s.normal.normalize_or(Vec3::Y);
        let heading = smooth_plane(s.heading, normal).normalize_or(Vec3::X);
        let prev = self.wheels[i].prev_contact;
        let velocity = prev.map_or(Vec3::ZERO, |p| (s.contact - p) / dt);
        let velocity = smooth_plane(velocity, normal);
        let v_long = velocity.dot(heading);
        let v_lat = (velocity - heading * v_long).length();
        let speed = velocity.length();
        let denom = v_long.abs().max(1.0);
        o.slip_ratio = if prev.is_some() {
            ((s.roll_speed - v_long) / denom).clamp(-2.0, 2.0)
        } else {
            0.0
        };
        o.slip_lateral = (v_lat / denom).min(2.0);
        o.slip = o.slip_ratio.hypot(o.slip_lateral).min(2.0);
        o.speed = speed;
        let cfg = self.config;
        let fx = s.surface.fx();
        // A spinning wheel on the spot still throws things: the faster of the two speeds counts.
        let effective = speed.max(s.roll_speed.abs());
        let scale = cfg.limits.effect_scale;
        if fx.smoke && o.slip > cfg.smoke_slip_threshold && effective > 1.0 {
            let excess =
                ((o.slip - cfg.smoke_slip_threshold) / (1.0 - cfg.smoke_slip_threshold)).clamp(0.0, 1.0);
            o.smoke = (excess * (effective / 20.0).clamp(0.2, 1.0) * scale).clamp(0.0, 1.0);
        }
        if fx.dust_gain > 0.0 && effective > cfg.min_speed {
            let rate = fx.dust_gain * effective * (1.0 + o.slip.min(1.0));
            o.dust = (rate * scale / DUST_RATE_MAX).clamp(0.0, 1.0);
        }
        if fx.spray_gain > 0.0 && effective > cfg.min_speed {
            o.spray = (fx.spray_gain * effective * scale / SPRAY_RATE_MAX).clamp(0.0, 1.0);
        }
        o.smoke_rate = o.smoke * SMOKE_RATE_MAX;
        o.dust_rate = o.dust * DUST_RATE_MAX;
        o.spray_rate = o.spray * SPRAY_RATE_MAX;

        let kind = match fx.mark {
            MarkKind::Skid => {
                let on = self.wheels[i].skid_on;
                let threshold = if on {
                    cfg.skid_slip_threshold * 0.8
                } else {
                    cfg.skid_slip_threshold
                };
                let now = o.slip > threshold && effective > cfg.min_speed;
                self.wheels[i].skid_on = now;
                if now { MarkKind::Skid } else { MarkKind::None }
            }
            MarkKind::Rut => {
                if speed > cfg.min_speed {
                    MarkKind::Rut
                } else {
                    MarkKind::None
                }
            }
            MarkKind::None => MarkKind::None,
        };
        o.mark = kind;
        if kind == MarkKind::None {
            self.wheels[i].run = None;
        } else {
            let dir = if speed > 0.1 { velocity / speed } else { heading };
            let opacity = match kind {
                MarkKind::Skid => fx.mark_opacity * (0.5 + 0.5 * (o.slip.min(1.0))),
                _ => fx.mark_opacity,
            };
            let style = StampStyle {
                normal,
                half_width: (s.half_width * fx.mark_width_scale).max(0.01),
                along: (cfg.spacing.max(0.05) * 1.05).max(0.01),
                color: fx.mark_color,
                opacity,
            };
            o.new_stamps = self.lay(i, s.contact, dir, &style, kind);
        }
        let w = &mut self.wheels[i];
        w.prev_contact = Some(s.contact);
        w.surface = s.surface;
        w.last = o;
        o
    }

    /// Extends wheel `i`'s run to the sample's contact; returns the stamps laid.
    fn lay(&mut self, i: usize, p: Vec3, dir: Vec3, style: &StampStyle, kind: MarkKind) -> u32 {
        let cfg = self.config;
        let spacing = cfg.spacing.max(0.05);
        let mut laid = 0u32;
        let run = self.wheels[i].run;
        let run = match run {
            Some(run) if run.kind == kind => run,
            _ => {
                self.push_stamp(p, dir, style);
                laid += 1;
                self.wheels[i].run = Some(Run {
                    last_vertex: p,
                    last_dir: dir,
                    to_next: spacing,
                    kind,
                });
                return laid;
            }
        };
        let delta = smooth_plane(p - run.last_vertex, style.normal);
        let dist = delta.length();
        let new_dir = if dist > 1e-5 { delta / dist } else { run.last_dir };
        let turned = new_dir.dot(run.last_dir).clamp(-1.0, 1.0).acos() > cfg.max_turn;
        let accept = dist >= spacing || (turned && dist >= spacing * 0.25);
        if !accept {
            return 0;
        }
        let mut t = run.to_next;
        while t <= dist {
            let at = run.last_vertex + new_dir * t;
            self.push_stamp(at, new_dir, style);
            laid += 1;
            t += spacing;
        }
        self.wheels[i].run = Some(Run {
            last_vertex: run.last_vertex + new_dir * dist,
            last_dir: new_dir,
            to_next: t - dist,
            kind,
        });
        laid
    }

    fn push_stamp(&mut self, at: Vec3, dir: Vec3, style: &StampStyle) {
        let life = self.config.mark_lifetime_s.max(0.1);
        let color = [style.color[0], style.color[1], style.color[2], style.opacity];
        let decal = Decal {
            position: at,
            rotation: align_along(style.normal, dir),
            half_size: Vec3::new(style.half_width, style.along, self.config.half_depth.max(0.01)),
            color,
            age: 0.0,
            lifetime: Some(life),
        };
        let surface = DecalSurface::from_decal(&decal);
        let serial = self.next_serial;
        self.next_serial += 1;
        self.stats.emitted += 1;
        self.ring.push_back((serial, self.clock));
        self.fresh.push(NewStamp {
            serial,
            decal,
            surface,
        });
    }

    /// A digest of everything that feeds the next step (FNV-1a over fixed-order fields), for
    /// determinism checks.
    #[must_use]
    pub fn state_digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |x: u64| {
            for b in x.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        };
        feed(self.next_serial);
        feed(u64::from(self.clock.to_bits()));
        for &(serial, born) in &self.ring {
            feed(serial);
            feed(u64::from(born.to_bits()));
        }
        for w in &self.wheels {
            feed(w.key);
            feed(u64::from(w.last.slip.to_bits()));
            feed(u64::from(w.last.smoke.to_bits()));
            if let Some(run) = w.run {
                feed(u64::from(run.last_vertex.x.to_bits()));
                feed(u64::from(run.last_vertex.y.to_bits()));
                feed(u64::from(run.last_vertex.z.to_bits()));
                feed(u64::from(run.to_next.to_bits()));
            }
        }
        h
    }
}

// ---------------------------------------------------------------------------
// Built-in effects
// ---------------------------------------------------------------------------

fn puff_sheet() -> FlipbookDef {
    FlipbookDef {
        cols: 8,
        rows: 2,
        frames: 16,
        mode: FlipbookMode::Fps(10.0),
        random_start: true,
        blend_frames: true,
    }
}

fn lod() -> LodDef {
    LodDef {
        distances: vec![40.0, 100.0, 250.0],
        rate_scale: vec![1.0, 0.5, 0.25, 0.0],
    }
}

/// The numbers of one billboard emitter.
struct Puffs {
    name: &'static str,
    rate: f32,
    life: (f32, f32),
    cone: f32,
    speed: (f32, f32),
    size: (f32, f32),
    color: &'static str,
    alpha: f32,
    grow: f32,
    gravity_y: f32,
    drag: f32,
    lit: bool,
}

fn emitter(p: &Puffs) -> EmitterDef {
    EmitterDef {
        name: p.name.into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(p.rate),
            lifetime: r(p.life.0, p.life.1),
            count_scale: 1.0,
        },
        init: InitModule {
            // The cone opens along local +Z, which the host points at the ground normal.
            shape: ShapeDef::Cone {
                angle: p.cone,
                length: r(0.05, 0.12),
            },
            velocity: VelocityDef {
                mode: VelocityMode::Radial {
                    speed: r(p.speed.0, p.speed.1),
                },
                inherit: 0.0,
            },
            size: r(p.size.0, p.size.1),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(-0.4, 0.4),
            color: ramp(&[(0.0, p.color)]),
        },
        update: UpdateModule {
            gravity: (0.0, p.gravity_y, 0.0),
            drag: p.drag,
            size_over_life: keys(&[(0.0, 0.6), (1.0, p.grow)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.12, p.alpha), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::PuffAnim,
            blend: BlendMode::Alpha,
            softness: 1.5,
            lit: p.lit,
            self_shadow: 0.4,
            screen_floor_px: 1.5,
            flipbook: Some(puff_sheet()),
            ..Default::default()
        },
    }
}

/// The built-in effect `name` ([`TIRE_SMOKE`], [`WHEEL_DUST`] or [`WHEEL_SPRAY`]); `None` for any
/// other name. Play it with local +Z on the ground normal, scale 1 and the intensity from
/// [`WheelOutput`].
#[must_use]
pub fn vehicle_effect(name: &str) -> Option<VfxSystemDef> {
    let emitters = match name {
        TIRE_SMOKE => vec![
            // Hot rubber smoke: pale grey-blue, rising slowly, spreading wide.
            emitter(&Puffs {
                name: "smoke",
                rate: SMOKE_RATE_MAX,
                life: (1.2, 2.2),
                cone: 28.0,
                speed: (0.6, 1.6),
                size: (0.35, 0.6),
                color: "#d6d8da",
                alpha: 0.5,
                grow: 3.2,
                gravity_y: 0.5,
                drag: 0.8,
                lit: true,
            }),
        ],
        WHEEL_DUST => vec![
            // A tan cloud lifted by the tyre and left behind.
            emitter(&Puffs {
                name: "cloud",
                rate: DUST_RATE_MAX,
                life: (1.0, 2.0),
                cone: 40.0,
                speed: (0.8, 2.4),
                size: (0.3, 0.6),
                color: "#b49d82",
                alpha: 0.45,
                grow: 3.0,
                gravity_y: 0.25,
                drag: 1.4,
                lit: true,
            }),
        ],
        WHEEL_SPRAY => vec![
            // Fine droplets and thrown mud: darker, shorter, flung outward faster.
            emitter(&Puffs {
                name: "droplets",
                rate: SPRAY_RATE_MAX,
                life: (0.5, 1.0),
                cone: 55.0,
                speed: (2.0, 5.0),
                size: (0.1, 0.22),
                color: "#9aa3a8",
                alpha: 0.55,
                grow: 1.6,
                gravity_y: -2.5,
                drag: 0.6,
                lit: false,
            }),
        ],
        _ => return None,
    };
    Some(VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: name.to_string(),
        system: SystemModule {
            bounds: None,
            budget: 512,
            lod: Some(lod()),
        },
        emitters,
    })
}

/// Adds built-in vehicle effect `name` to `scene`'s library when it lacks it. `false` when `name`
/// is not a vehicle effect or the library rejects it; a game's own def of the same name is kept.
pub fn ensure_vehicle_effect(scene: &mut crate::scene::VfxScene, name: &str) -> bool {
    if scene.library().index(name).is_some() {
        return true;
    }
    let Some(def) = vehicle_effect(name) else {
        return false;
    };
    match scene.library_mut().add(def) {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(target: "regia_vfx", "built-in effect {name} rejected: {err}");
            false
        }
    }
}

#[cfg(test)]
mod tests;
