// SPDX-License-Identifier: MPL-2.0
//! Crash and impact effects that scale from a vehicle crash to a large surface explosion
//! (ADR-1398, rescaled by ADR-1414): a fireball with a flash that lights the land, an optional
//! condensation shell, a blast ring that runs out at the speed of sound and lifts the ground
//! cover, a base surge, an ejecta fountain, a mushroom column (cap and stem), ground fires, and
//! a small dust puff at first ground contact.
//!
//! One parameter sizes everything: [`CrashScale`], the TNT-equivalent mass of the energy
//! release, from 0.05 t (a light vehicle) to 1 kt. Every layer's definition is generated for a
//! scale and registered under a name that carries it (`crash_fireball@250t`), so a small and a
//! large crash can play in one scene. The layers play through the normal [`VfxScene`] path
//! (`.vfx.ron` v5 definitions, the GPU pool, the soft layer and the particle lights), so every
//! host that draws `regia_vfx` draws them.
//!
//! Sizes and timings follow public physical scaling (`docs/provenance.md`):
//!
//! - **Cube-root (Hopkinson–Cranz) scaling** for lengths: with `W` in kg TNT, the fireball's
//!   largest diameter is about `3.5 W^(1/3)` m, the apparent crater in soil about
//!   `0.8 W^(1/3)` m across ([`CraterSpec`]), and every other reach is a fixed multiple of
//!   `W^(1/3)`. 250 t gives a 220 m fireball and a 50 m crater.
//! - **Froude (buoyancy) scaling** for the fireball's timing and rise: the time scale is
//!   `τ = √(R/g)` and the speed scale `√(gR)` for the fireball radius `R`, so the ball peaks at
//!   about `0.37 τ` (1.2 s at 250 t) and rises at about `0.7 √(gR)` (24 m/s); the fireball
//!   itself lives in its own module (ADR-1498, [`crash_fireball`]).
//! - **The speed of sound** for the blast front: 343 m/s at 20 °C at every scale, never faster
//!   than the front for anything the blast lifts.
//! - **Ballistic flight** for the ejecta: thrown at `1.2–2.6 √(gR)`, 45 to 90 degrees up, they
//!   reach `v² / 2g` and land at `v² sin 2θ / g` (300 m up and 500–750 m out at 250 t).
//! - **A buoyant thermal in a stable atmosphere** for the cap: it rises to a height that grows
//!   as `W^(1/4)` and stops there (Morton, Taylor & Turner 1956), modelled as a launch that
//!   decelerates with a 30 s time constant (1.1 km at 250 t, about 25 m/s over the first 30 s).
//!
//! Hard rules the layers keep (the showcase's physics check): debris has no flame trails and
//! nothing glows near the ground except the fires, nothing outruns the blast front, and a
//! listener feels the blast only when the front reaches it ([`CrashSequence`]).
//!
//! ## Tiers
//!
//! [`CrashTier::Full`] is the desktop look. [`CrashTier::Lite`] is the cheap fallback for Low,
//! the Steam Deck and mobile ([`CrashTier::for_profile`]): fewer, larger puffs with the same
//! envelope, no heat haze and no refraction ring (both need a scene copy).
//!
//! ## Frame and hooks
//!
//! Every layer plays with local +Z on the ground normal. [`spawn_crash`] fires the detonation
//! layers at once; [`CrashSequence`] (driven by [`drive_crash_sequences`] in `VfxPlugin`) fires
//! the contact puff a configurable lead before the detonation and sends [`CrashEvent`]s:
//! `Contact`, `Detonation`, `Crater` (the crater's size for a decal or terrain lane, and the
//! point where a battered hull belongs) and `BlastArrived` (with a camera shake).
//!
//! Wind is folded into each emitter's acceleration (`g + k·w`, as in
//! [`crate::atmos_presets`]), so the definitions are built for one wind; [`ensure_crash_effects`]
//! re-registers them when the wind changes.

use regia_app::prelude::*;
use regia_core::math::{Vec3, det_cbrt, det_sin_cos};

use crate::atmos_presets::{drift, keys, r, ramp, settle};
use crate::format::{
    BlendMode, EmitterDef, FlipbookDef, FlipbookMode, InitModule, LodDef, ParticleLightDef, RenderMode,
    RenderModule, ShapeDef, SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION,
    VelocityDef, VelocityMode, VfxSystemDef,
};
use crate::gpu::{SoftParticleRes, VfxProfile};
use crate::medium::VfxMedium;
use crate::scene::VfxScene;
use crate::shake::CameraShakeRequest;

mod front;
pub use front::*;

/// Base name of the fireball effect (`crash_fireball@<scale>`).
pub const CRASH_FIREBALL: &str = "crash_fireball";
/// Base name of the condensation-shell effect (`crash_shell@<scale>`).
pub const CRASH_SHELL: &str = "crash_shell";
/// Base name of the mushroom-column effect (`crash_column@<scale>`).
pub const CRASH_COLUMN: &str = "crash_column";
/// Speed of the blast front far from the burst, m/s (sound at 20 °C). One constant shared with
/// the ground-shock airblast: re-exported from `regia_core::SPEED_OF_SOUND_M_S` (ADR-1497) so a
/// server build without this crate still sees the same value.
pub use regia_core::SPEED_OF_SOUND_M_S as SHOCK_FRONT_SPEED;
/// Name prefix of the dust wall's ring emitters in `crash_dust_<ground>@<scale>`
/// (`front_wall_0`, `front_wall_1`, …, inside out; ADR-1497).
pub const WALL_EMITTER_PREFIX: &str = "front_wall_";
/// Ratio of one dust-wall ring's radius to the next one in ([`CrashTier::Full`]; Lite uses
/// 1.15). A ring's puffs come to rest between it and the ring inside it, so the wall's edge is
/// never more than this ratio behind the front.
pub const WALL_RING_RATIO: f32 = 1.075;
/// The gust puff a branch or a patch of loose snow sheds as the blast front passes
/// (`crash_gust_snow`, ADR-1497): scale-free, sized for a tree crown.
pub const CRASH_GUST_SNOW: &str = "crash_gust_snow";
/// Light intensity units of the flash of a 250 t crash (a point light of `intensity ×
/// PARTICLE_LIGHT_LUMENS_PER_UNIT` lumens; scaled with the fireball's surface area).
pub const FLASH_INTENSITY_250T: f32 = 4.0e5;

const G: f32 = 9.81;
/// The dust wall's first ring, metres per `W^(1/3)`.
const WALL_INNER: f32 = 2.2;
/// [`WALL_RING_RATIO`] on Lite.
const WALL_RING_RATIO_LITE: f32 = 1.15;
/// Drag of a dust-wall puff, 1/s. A puff leaves the crash point at `WALL_DRAG × r` and the
/// kernel's implicit drag (`v ← v / (1 + k dt)`) stops it at exactly `r` (the geometric sum of
/// its steps), 80 % of the way there after one 60 Hz frame and 96 % after two; it then rises at
/// its terminal speed and drifts with the wind.
const WALL_DRAG: f32 = 240.0;
/// Fireball radius of the 250 t reference, metres (`1.75 · 250 000^(1/3)`).
const R_250T: f32 = 110.24;

/// Which crash look a device can afford.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum CrashTier {
    /// Desktop: every layer (billows, heat haze, refraction front, fines).
    #[default]
    Full,
    /// Low, Steam Deck and mobile: fewer, larger puffs, no haze or refraction. Same size,
    /// timing and colour envelope.
    Lite,
}

impl CrashTier {
    /// Both tiers.
    pub const ALL: [Self; 2] = [Self::Full, Self::Lite];

    /// The tier a VFX profile affords: [`Self::Lite`] below 50 000 live particles, under a
    /// 4 096 smoke budget or with a quarter-resolution soft layer (the mobile and Deck
    /// profiles), else [`Self::Full`].
    #[must_use]
    pub fn for_profile(profile: &VfxProfile) -> Self {
        if profile.particle_capacity < 50_000
            || profile.smoke_budget < 4_096
            || profile.soft_particle_res == SoftParticleRes::Quarter
        {
            Self::Lite
        } else {
            Self::Full
        }
    }

    /// Lower-case name (`full`, `lite`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lite => "lite",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    fn pick<T>(self, full: T, lite: T) -> T {
        match self {
            Self::Full => full,
            Self::Lite => lite,
        }
    }
}

/// The loose ground a crash throws up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum GroundClass {
    /// Wet soil and mud: dark brown, heavy clods, a brown-grey dust.
    #[default]
    Mud,
    /// Snow over frozen ground: white powder that hangs and settles slowly; the crater digs
    /// into the earth under it.
    Snow,
    /// Dry grass and topsoil: straw-coloured sods and a pale dust.
    Grass,
}

impl GroundClass {
    /// Every class.
    pub const ALL: [Self; 3] = [Self::Mud, Self::Snow, Self::Grass];

    /// Lower-case name (`mud`, `snow`, `grass`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Mud => "mud",
            Self::Snow => "snow",
            Self::Grass => "grass",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The blast-and-surge effect for this ground at `scale` (`crash_dust_mud@250t`, …).
    #[must_use]
    pub fn dust_effect(self, scale: CrashScale) -> String {
        format!("crash_dust_{}@{}", self.name(), scale.tag())
    }

    /// The ejecta effect for this ground at `scale` (`crash_ejecta_mud@250t`, …).
    #[must_use]
    pub fn ejecta_effect(self, scale: CrashScale) -> String {
        format!("crash_ejecta_{}@{}", self.name(), scale.tag())
    }

    /// The first-contact puff for this ground (`crash_contact_mud`, …). It is sized for the
    /// falling body, not the yield.
    #[must_use]
    pub fn contact_effect(self) -> &'static str {
        match self {
            Self::Mud => "crash_contact_mud",
            Self::Snow => "crash_contact_snow",
            Self::Grass => "crash_contact_grass",
        }
    }

    /// The loose-ground class that best matches a ground albedo (linear RGB, e.g. the terrain
    /// layer under the crash point): bright, low-saturation cover is [`Self::Snow`], a green or
    /// yellow-dominant mid tone is [`Self::Grass`], anything else [`Self::Mud`]. The dust skirt
    /// then takes the receiving ground's colour (snow stays white-grey, ADR-1444).
    #[must_use]
    pub fn from_albedo(rgb: [f32; 3]) -> Self {
        let [r, g, b] = rgb;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let hi = r.max(g).max(b);
        let lo = r.min(g).min(b);
        let chroma = if hi > 1e-6 { (hi - lo) / hi } else { 0.0 };
        if luma >= 0.35 && chroma < 0.2 {
            Self::Snow
        } else if g >= b && g >= 0.9 * r && chroma > 0.15 {
            Self::Grass
        } else {
            Self::Mud
        }
    }

    /// `(dust, clod, ejecta)` sRGB colours. The ejecta is earth from under the cover.
    fn colors(self) -> (&'static str, &'static str, &'static str) {
        match self {
            // Wet loam: dark brown clods, a brown-grey dust (soil Munsell 10YR 3/2 to 5/2).
            Self::Mud => ("#7d7466", "#3b3026", "#3b3026"),
            // Snow: near white powder; the clods and the fountain are frozen earth with snow.
            Self::Snow => ("#d9dcde", "#4d443a", "#4d443a"),
            // Dry grass and dusty topsoil: straw sods, a pale tan dust, brown earth below.
            Self::Grass => ("#9a8f78", "#7a6a45", "#5a4a33"),
        }
    }

    /// Opacity of the blast-lifted cover relative to soil dust (ADR-1425). Snow powder is a
    /// thin, translucent cloud of grains (bulk density of new snow 50–100 kg/m³ against
    /// 1,100–1,600 for dry soil, so far fewer grains per metre of plume for the same lift), so
    /// its surge reads as a pale veil with the ground through it, not a wall of puffs.
    fn dust_opacity(self) -> f32 {
        match self {
            Self::Snow => 0.55,
            Self::Mud | Self::Grass => 1.0,
        }
    }

    /// Settling speed of the blast-lifted cover, m/s (negative: down): snow powder settles at
    /// about 1 m/s (Locatelli & Hobbs 1974), fine dust hangs.
    fn lifted_settle(self) -> f32 {
        match self {
            Self::Snow => -0.8,
            Self::Mud | Self::Grass => -0.3,
        }
    }
}

/// The size of a crash: the TNT-equivalent mass of its energy release, in tonnes (cube-root
/// scaling, see the module docs). Quantised to three significant digits so that a scale names
/// its effects stably.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct CrashScale {
    tonnes: f32,
}

impl Default for CrashScale {
    fn default() -> Self {
        Self::VEHICLE
    }
}

/// The crater a crash leaves, from its [`CrashScale`] (near-surface burst in soil): for a decal,
/// terrain or wreck lane to apply. Every length in metres.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct CraterSpec {
    /// Apparent crater diameter at the original ground level (`0.8 W^(1/3)`).
    pub diameter: f32,
    /// Apparent depth below the original ground (about 0.28 × the diameter).
    pub depth: f32,
    /// Raised rim height (about 0.08 × the diameter).
    pub rim_height: f32,
    /// Radius of the continuous ejecta blanket (about 3 × the diameter).
    pub ejecta_blanket_radius: f32,
    /// Radius of scorch and stripped snow (the fire zone).
    pub scorch_radius: f32,
}

impl CrashScale {
    /// Smallest scale, tonnes TNT.
    pub const MIN_TONNES: f32 = 0.05;
    /// Largest scale, tonnes TNT (1 kt).
    pub const MAX_TONNES: f32 = 1_000.0;
    /// A vehicle crash: about a tonne (a 35 m fireball).
    pub const VEHICLE: Self = Self { tonnes: 1.0 };
    /// The showcase impact: 0.25 kt (a 220 m fireball, a 50 m crater, a 1.1 km column).
    pub const SHOWCASE: Self = Self { tonnes: 250.0 };
    /// The largest scale, 1 kt ([`Self::MAX_TONNES`]).
    pub const LARGEST: Self = Self {
        tonnes: Self::MAX_TONNES,
    };

    /// The scale of `tonnes` TNT-equivalent, clamped to `MIN_TONNES..=MAX_TONNES` and rounded
    /// to three significant digits (a non-finite input gives [`Self::VEHICLE`]).
    #[must_use]
    pub fn from_tonnes(tonnes: f32) -> Self {
        if !tonnes.is_finite() {
            return Self::VEHICLE;
        }
        let t = tonnes.clamp(Self::MIN_TONNES, Self::MAX_TONNES);
        let digits = 2 - t.log10().floor() as i32;
        let tonnes = if digits > 0 {
            let k = 10f32.powi(digits);
            (t * k).round() / k
        } else {
            let k = 10f32.powi(-digits);
            (t / k).round() * k
        };
        Self { tonnes }
    }

    /// TNT-equivalent tonnes.
    #[must_use]
    pub fn tonnes(self) -> f32 {
        self.tonnes
    }

    /// The name tag of this scale (`250t`, `0.5t`).
    #[must_use]
    pub fn tag(self) -> String {
        format!("{}t", self.tonnes)
    }

    /// Parses an effect name's scale tag (`crash_fireball@250t` → 250 t). `None` without a
    /// canonical tag.
    #[must_use]
    pub fn from_effect_name(name: &str) -> Option<Self> {
        let (_, tag) = name.rsplit_once('@')?;
        let tonnes: f32 = tag.strip_suffix('t')?.parse().ok()?;
        let scale = Self::from_tonnes(tonnes);
        (scale.tag() == tag).then_some(scale)
    }

    /// `W^(1/3)` with `W` in kg TNT. The deterministic cube-root kernel, not the platform
    /// libm: the radii feed world positions and pinned offset bits, so a 1-ulp `cbrt`
    /// difference between libms would move them (fixrev-22-k, Muse review finding 2).
    fn cbrt_kg(self) -> f32 {
        det_cbrt(self.tonnes * 1_000.0)
    }

    /// Largest fireball radius, metres (`1.75 W^(1/3)`: a diameter of `3.5 W^(1/3)`).
    #[must_use]
    pub fn fireball_radius(self) -> f32 {
        1.75 * self.cbrt_kg()
    }

    /// Buoyancy time scale `√(R/g)`, seconds.
    #[must_use]
    pub fn time_scale(self) -> f32 {
        (self.fireball_radius() / G).sqrt()
    }

    /// Buoyancy speed scale `√(gR)`, m/s.
    #[must_use]
    pub fn speed_scale(self) -> f32 {
        (G * self.fireball_radius()).sqrt()
    }

    /// How far the visible blast ring runs, metres (`32 W^(1/3)`: 2 km at 250 t).
    #[must_use]
    pub fn blast_reach(self) -> f32 {
        32.0 * self.cbrt_kg()
    }

    /// How far the blast lifts snow and dust, metres (`6.4 W^(1/3)`: 400 m at 250 t).
    #[must_use]
    pub fn lift_reach(self) -> f32 {
        6.4 * self.cbrt_kg()
    }

    /// How far the dust wall runs behind the front, metres (`1.15 ×` [`Self::lift_reach`]:
    /// 464 m at 250 t). Past it the front shows only as the gust it drags across the vegetation
    /// ([`blast_gust`], ADR-1497).
    #[must_use]
    pub fn wall_reach(self) -> f32 {
        1.15 * self.lift_reach()
    }

    /// Radii of the dust wall's rings for `tier`, inside out, metres: from `2.2 W^(1/3)` (139 m
    /// at 250 t, so the wall is up by 150 m) to [`Self::wall_reach`], each [`WALL_RING_RATIO`]
    /// (Full) or 1.15 (Lite) times the one inside it.
    #[must_use]
    pub fn wall_rings(self, tier: CrashTier) -> Vec<f32> {
        let ratio = tier.pick(WALL_RING_RATIO, WALL_RING_RATIO_LITE);
        let reach = self.wall_reach() * 1.000_1;
        let mut out = Vec::new();
        let mut r = WALL_INNER * self.cbrt_kg();
        while r <= reach {
            out.push(r);
            r *= ratio;
        }
        out
    }

    /// Condensation-shell radius in humid air, metres (`3.2 W^(1/3)`: 200 m at 250 t).
    #[must_use]
    pub fn shell_radius(self) -> f32 {
        3.2 * self.cbrt_kg()
    }

    /// Height where the cap stops rising, metres (`50 W^(1/4)`: 1.1 km at 250 t).
    #[must_use]
    pub fn column_height(self) -> f32 {
        50.0 * (self.tonnes * 1_000.0).powf(0.25)
    }

    /// Radius of the zone the blast sets alight, metres (`4.8 W^(1/3)`: 300 m at 250 t).
    #[must_use]
    pub fn fire_zone(self) -> f32 {
        4.8 * self.cbrt_kg()
    }

    /// Ground fires the blast lights: about 55 at 250 t, growing with the fire zone's area,
    /// 3 to 80.
    #[must_use]
    pub fn fire_count(self) -> u32 {
        let k = self.fireball_radius() / R_250T;
        (55.0 * k * k).round().clamp(3.0, 80.0) as u32
    }

    /// The crater this crash leaves.
    #[must_use]
    pub fn crater(self) -> CraterSpec {
        let diameter = 0.8 * self.cbrt_kg();
        CraterSpec {
            diameter,
            depth: 0.28 * diameter,
            rim_height: 0.08 * diameter,
            ejecta_blanket_radius: 3.0 * diameter,
            scorch_radius: self.fire_zone(),
        }
    }

    /// How the crater throws its earth and rock: the ballistic launch envelope the `clods`
    /// layer uses, for a debris system that flies mesh pieces on the same arcs.
    #[must_use]
    pub fn ejecta_launch(self) -> EjectaLaunch {
        let u = self.speed_scale();
        EjectaLaunch {
            min_speed: 1.2 * u,
            max_speed: 2.6 * u,
            cone_degrees: 45.0,
        }
    }

    /// Camera-shake trauma when the blast front reaches a listener `distance` metres away:
    /// `12 / Z` for the scaled distance `Z = d / W^(1/3)` (the far-field overpressure falls
    /// about as `1/Z`), at most 1.
    #[must_use]
    pub fn blast_shake(self, distance: f32) -> f32 {
        let z = distance.max(1.0) / self.cbrt_kg();
        (12.0 / z).min(1.0)
    }

    /// Ratio of this scale's fireball to a 20 m one, at least 1: LOD distances grow with it.
    fn lod_factor(self) -> f32 {
        (self.fireball_radius() / 20.0).max(1.0)
    }
}

/// The ballistic launch of a crash's ejecta ([`CrashScale::ejecta_launch`]): speeds and the
/// half-angle of the cone about the ground normal. With no drag a piece reaches `v² cos²θ / 2g`
/// and lands `v² sin 2θ' / g` out (`θ'` its elevation).
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct EjectaLaunch {
    /// Slowest launch, m/s.
    pub min_speed: f32,
    /// Fastest launch, m/s.
    pub max_speed: f32,
    /// Half-angle of the launch cone about the ground normal, degrees.
    pub cone_degrees: f32,
}

/// A ground-fire size (`crash_ground_fire_<size>`): bigger fires burn longer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum FireSize {
    /// Grass and brush: 1–2 m flames, burns 200 s.
    #[default]
    Small,
    /// A bush or a stump: 2–3.5 m flames, burns 300 s.
    Medium,
    /// A tree: 3–5.5 m flames, burns 420 s.
    Large,
}

impl FireSize {
    /// Every size.
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    /// Lower-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    /// The effect name (`crash_ground_fire_small`, …).
    #[must_use]
    pub fn effect(self) -> &'static str {
        match self {
            Self::Small => "crash_ground_fire_small",
            Self::Medium => "crash_ground_fire_medium",
            Self::Large => "crash_ground_fire_large",
        }
    }

    /// How long the fire burns, seconds.
    #[must_use]
    pub fn burn_seconds(self) -> f32 {
        match self {
            Self::Small => 200.0,
            Self::Medium => 300.0,
            Self::Large => 420.0,
        }
    }

    /// `(flame size range, flame rate, smoke rate, light intensity)`. Few, long-lived flame
    /// cards: 55 fires share the particle ring with the 4-minute cap (see `crash_column`).
    fn look(self) -> ((f32, f32), f32, f32, f32) {
        match self {
            Self::Small => ((1.4, 2.4), 1.2, 0.4, 4.0),
            Self::Medium => ((2.2, 3.8), 2.0, 0.6, 7.0),
            Self::Large => ((3.2, 5.8), 3.0, 0.8, 12.0),
        }
    }
}

/// An 8-frame fireball-body sheet (atlas tiles 32..40), random start, blended frames.
fn fireball_sheet() -> FlipbookDef {
    FlipbookDef {
        cols: 8,
        rows: 1,
        frames: 8,
        mode: FlipbookMode::Fps(8.0),
        random_start: true,
        blend_frames: true,
    }
}

/// The 8-frame flame sheet (atlas tiles 24..32), random start, blended frames.
fn flame_sheet() -> FlipbookDef {
    FlipbookDef {
        cols: 8,
        rows: 1,
        frames: 8,
        mode: FlipbookMode::Fps(12.0),
        random_start: true,
        blend_frames: true,
    }
}

/// Full detail out to `near` metres, thinner (larger puffs, [`crate::sim::lod_size_compensation`])
/// to `far`, off past it. Both grow with the crash, so a large one keeps full detail across a
/// valley and nothing pops in a shot that stays inside one band.
fn lod(scale: CrashScale, near: f32, far: f32) -> Option<LodDef> {
    let k = scale.lod_factor();
    Some(LodDef {
        distances: vec![near * k, far * k],
        rate_scale: vec![1.0, 0.6, 0.0],
    })
}

fn burst(n: u32, life: (f32, f32)) -> SpawnModule {
    SpawnModule {
        mode: SpawnMode::Burst(n),
        lifetime: r(life.0, life.1),
        count_scale: 1.0,
    }
}

fn rate(n: f32, life: (f32, f32)) -> SpawnModule {
    SpawnModule {
        mode: SpawnMode::Rate(n),
        lifetime: r(life.0, life.1),
        count_scale: 1.0,
    }
}

/// Spawn positions on a disc `height` above the origin along local +Z, out to `height ×
/// tan(angle)` (area-uniform: most near the rim).
fn disc(angle: f32, height: (f32, f32)) -> ShapeDef {
    ShapeDef::Cone {
        angle,
        length: r(height.0, height.1),
    }
}

fn planar(speed: (f32, f32), lift: (f32, f32)) -> VelocityDef {
    VelocityDef {
        mode: VelocityMode::Planar {
            speed: r(speed.0, speed.1),
            lift: r(lift.0, lift.1),
        },
        inherit: 0.0,
    }
}

/// Up along the ground normal (local +Z) within `spread` degrees.
fn upward(spread: f32, speed: (f32, f32)) -> VelocityDef {
    VelocityDef {
        mode: VelocityMode::Directional {
            dir: (0.0, 0.0, 1.0),
            spread,
            speed: r(speed.0, speed.1),
        },
        inherit: 0.0,
    }
}

fn init(shape: ShapeDef, velocity: VelocityDef, size: (f32, f32), color: &str) -> InitModule {
    InitModule {
        shape,
        velocity,
        size: r(size.0, size.1),
        rotation: r(0.0, std::f32::consts::TAU),
        spin: r(-0.3, 0.3),
        color: ramp(&[(0.0, color)]),
    }
}

/// Life fraction up to which a cap puff is "young": its size and opacity keys are unchanged.
pub(crate) const CAP_YOUNG_UNTIL: f32 = 1.0 / 7.0;
/// Extra growth of an aging cap puff (look round 5): by the end of its life a puff is
/// `1 + CAP_AGE_DIFFUSION` times its old size, with its opacity divided by the same factor, so the
/// old cap spreads into a wider, softer mass instead of keeping a crisp rim. The factor ramps
/// linearly from 1 at [`CAP_YOUNG_UNTIL`]; 0.0 is the old look.
pub(crate) const CAP_AGE_DIFFUSION: f32 = 0.3;

/// How much wider (and so softer) a cap puff is at life fraction `t` than its authored size:
/// exactly 1 up to [`CAP_YOUNG_UNTIL`], then rising linearly to `1 + diffusion` at the end of life.
pub(crate) fn cap_age_growth(t: f32, diffusion: f32) -> f32 {
    1.0 + diffusion * ((t - CAP_YOUNG_UNTIL) / (1.0 - CAP_YOUNG_UNTIL)).clamp(0.0, 1.0)
}

/// Scales keys of a cap puff by its age: sizes grow by [`cap_age_growth`], opacities shrink by it
/// (`grow` false). Keys at or before [`CAP_YOUNG_UNTIL`] are returned unchanged.
pub(crate) fn cap_aged_keys(points: &[(f32, f32)], diffusion: f32, grow: bool) -> Vec<(f32, f32)> {
    points
        .iter()
        .map(|&(t, v)| {
            let g = cap_age_growth(t, diffusion);
            (t, if grow { v * g } else { v / g })
        })
        .collect()
}

/// A lit, soft smoke or dust puff.
fn lit_puff(softness: f32, self_shadow: f32) -> RenderModule {
    RenderModule {
        mode: RenderMode::Billboard,
        sprite: SpriteId::PuffAnim,
        flipbook: Some(crate::showcase::puff_flipbook()),
        blend: BlendMode::Alpha,
        softness,
        emissive: 1.0,
        lit: true,
        self_shadow,
        screen_floor_px: 2.0,
        ..RenderModule::default()
    }
}

/// A tumbling clod, sod or rock (unlit by fire: no glow, no trail).
fn clod_mesh() -> RenderModule {
    RenderModule {
        mode: RenderMode::Mesh,
        sprite: SpriteId::Shard,
        blend: BlendMode::Alpha,
        ..RenderModule::default()
    }
}

/// Albedo over life of the fireball's soot (sRGB hex of the linear albedo; the init colour is
/// white so the ramp is the albedo): carbon black, 0.011 at birth and 0.03 at 1/5 of its life
/// (past `τ` on the shortest life), then 0.12 at 3/10, 0.18 at 9/20 and 0.3 at the end as it
/// mixes into the condensation (ADR-1472).
pub(crate) const SOOT_MIX_ALBEDO: [(f32, &str); 5] = [
    (0.0, "#1b1b1b"),
    (0.2, "#303030"),
    (0.3, "#616161"),
    (0.45, "#767676"),
    (1.0, "#959595"),
];

/// Albedo over life of the mushroom cap (ADR-1472). An energy-store vent over snow lifts water
/// vapour that condenses (albedo near 1), snow powder (about 0.8) and some soil dust and soot,
/// so the cap is a grey-white mass, not a soot cloud: 0.42 as it leaves the fireball, 0.55 by
/// about 9 s, 0.62 by 23 s (the end of the showcase shot), 0.75 once it has diluted. The cap
/// lives 200–260 s, so the shot sees only the first tenth of the ramp.
pub(crate) const CAP_ALBEDO: [(f32, &str); 4] = [
    (0.0, "#adadad"),
    (0.04, "#c4c4c4"),
    (0.1, "#cecece"),
    (1.0, "#e1e1e1"),
];

/// Albedo over life of the stem (ADR-1472): dirty snow and soil dust drawn up from the crater
/// (0.22 at the crater, which also holds the hardware's smoke) that dilutes and takes
/// condensation as it rises (0.4 at 30 % of its rise, 0.6 under the cap).
pub(crate) const STEM_ALBEDO: [(f32, &str); 3] = [(0.0, "#818181"), (0.3, "#aaaaaa"), (1.0, "#cbcbcb")];

/// The neutral cooling tint of a fire body: the sheet carries the hue, the tint only dims it
/// (linear 0.27 × emissive 6 stays under the `Rgba16Float` ceiling at EV 15, as for
/// [`crate::showcase::explosion`]).
fn cooling_tint(end: &str) -> Vec<crate::format::ColorKey> {
    ramp(&[
        (0.0, "#8e8e8e"),
        (2.0 / 7.0, "#8a8a8a"),
        (4.0 / 7.0, "#5c5c5c"),
        (1.0, end),
    ])
}

fn system(name: String, budget: u32, lod: Option<LodDef>, emitters: Vec<EmitterDef>) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name,
        system: SystemModule {
            bounds: None,
            budget,
            lod,
        },
        emitters,
    }
}

/// The fireball name at `scale` (`crash_fireball@250t`).
#[must_use]
pub fn fireball_effect(scale: CrashScale) -> String {
    format!("{CRASH_FIREBALL}@{}", scale.tag())
}

/// The condensation-shell name at `scale` (`crash_shell@250t`).
#[must_use]
pub fn shell_effect(scale: CrashScale) -> String {
    format!("{CRASH_SHELL}@{}", scale.tag())
}

/// The column name at `scale` (`crash_column@250t`).
#[must_use]
pub fn column_effect(scale: CrashScale) -> String {
    format!("{CRASH_COLUMN}@{}", scale.tag())
}

pub use fireball::{
    FIREBALL_SPAWN_HEIGHT, FLAME_VISIBLE_TINT, FireballFootprint, PUFF_BODY_RADIUS, SHEET_FLAME_RADIUS,
    crash_fireball, fire_light_falloff, fireball_footprint,
};

/// The condensation shell (`crash_shell@<scale>`): in humid air the rarefaction behind the
/// blast front condenses a thin white hemisphere about `3.2 W^(1/3)` out (200 m at 250 t). It
/// appears as the front passes and is gone within about 0.8 s. Spawn on the ground point.
#[must_use]
pub fn crash_shell(scale: CrashScale, tier: CrashTier) -> VfxSystemDef {
    let rc = scale.shell_radius();
    let pass = rc / SHOCK_FRONT_SPEED;
    let life = 0.6 * pass + 0.06;
    system(
        shell_effect(scale),
        256,
        lod(scale, 1_500.0, 4_000.0),
        vec![EmitterDef {
            name: "shell".into(),
            delay: 0.5 * pass,
            duration: None,
            spawn: burst(tier.pick(110, 48), (0.85 * life, life)),
            init: init(
                ShapeDef::Sphere {
                    radius: r(0.9 * rc, rc),
                    surface: true,
                },
                upward(0.0, (0.0, 0.0)),
                tier.pick((0.3 * rc, 0.42 * rc), (0.4 * rc, 0.55 * rc)),
                "#f4f6f8",
            ),
            update: UpdateModule {
                size_over_life: keys(&[(0.0, 0.8), (1.0, 1.15)]),
                alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.55), (4.0 / 7.0, 0.4), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: lit_puff(0.1 * rc, 0.3),
        }],
    )
}

/// The blast ring and base surge for `ground` (`crash_dust_<ground>@<scale>`):
///
/// - `blast_front`: a thin skirt of snow and dust kicked up at the front, running at
///   [`SHOCK_FRONT_SPEED`] out to [`CrashScale::blast_reach`], thick to the lift reach and
///   fading as the overpressure falls. Alpha cards in the sorted list (a refraction ring drawn
///   from the unsorted haze list would blend in a different order on every run); nothing is a
///   camera-facing ring sprite.
/// - `lifted`: the snow or dust the blast wind lifts behind the front, carried outward and
///   dropped by drag out to [`CrashScale::lift_reach`]; never faster than the front.
/// - `surge` and `surge_billow` (Full): the base surge, a ring of dust that runs out along the
///   ground and slows (`1.2–2.6 √(gR)` at first, a `0.4/τ` drag): 400 to 700 m out and 50 to
///   100 m high about 15 s after a 250 t blast.
///
/// Spawn on the ground point with `forward` on the normal.
#[must_use]
pub fn crash_dust(
    ground: GroundClass,
    scale: CrashScale,
    tier: CrashTier,
    medium: &VfxMedium,
) -> VfxSystemDef {
    let (dust, _, _) = ground.colors();
    let big_r = scale.fireball_radius();
    let tau = scale.time_scale();
    let u = scale.speed_scale();
    let reach = scale.blast_reach();
    let ring_life = reach / SHOCK_FRONT_SPEED;
    let lift_reach = scale.lift_reach();
    let lift_drag = 0.96 * SHOCK_FRONT_SPEED / (1.05 * lift_reach);
    let surge_drag = 0.4 / tau;
    let mut emitters = vec![EmitterDef {
        // The front itself: a thin skirt of snow and dust kicked up right where the front is,
        // running out at the speed of sound. Thick to the lift reach, then fainter as the
        // overpressure falls, gone near the ring's reach. Alpha cards (the sorted list), so
        // two runs blend them in the same order; never a camera-facing ring sprite.
        name: "blast_front".into(),
        delay: 0.0,
        duration: None,
        spawn: burst(tier.pick(720, 240), (0.97 * ring_life, ring_life)),
        init: init(
            disc(84.0, (0.5, 0.6)),
            // The dust a front raises trails just behind it: a band from 85 % of the front
            // speed to the front itself, so it reads as a low ripple, not a drawn line.
            planar((0.85 * SHOCK_FRONT_SPEED, SHOCK_FRONT_SPEED + 3.0), (0.0, 4.0)),
            tier.pick((0.02 * reach, 0.025 * reach), (0.03 * reach, 0.035 * reach)),
            dust,
        ),
        update: UpdateModule {
            size_over_life: keys(&[(0.0, 0.1), (1.0, 1.0)]),
            alpha_over_life: keys(&[
                (0.0, 0.0),
                (1.0 / 7.0, 0.3),
                (3.0 / 7.0, 0.07),
                (6.0 / 7.0, 0.065),
                (1.0, 0.035),
            ]),
            ..UpdateModule::default()
        },
        render: lit_puff(0.01 * reach, 0.5),
    }];
    emitters.push(EmitterDef {
        // Snow and dust the blast wind lifts behind the front: launched outward at up to 96 %
        // of the front speed and stopped by drag, so it never leads the front.
        name: "lifted".into(),
        delay: 0.0,
        duration: None,
        spawn: burst(tier.pick(260, 96), (3.0 * tau, 4.5 * tau)),
        init: init(
            disc(80.0, (0.3, 0.5)),
            planar(
                (0.3 * SHOCK_FRONT_SPEED, 0.96 * SHOCK_FRONT_SPEED),
                (0.02 * u, 0.12 * u),
            ),
            tier.pick(
                (0.04 * lift_reach, 0.07 * lift_reach),
                (0.06 * lift_reach, 0.09 * lift_reach),
            ),
            dust,
        ),
        update: UpdateModule {
            gravity: drift(medium, ground.lifted_settle() * lift_drag, lift_drag),
            drag: lift_drag,
            curl: Some((1.2, 0.05 / big_r.sqrt(), 0.2)),
            size_over_life: keys(&[(0.0, 0.5), (1.0 / 7.0, 1.2), (1.0, 2.2)]),
            alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.4), (4.0 / 7.0, 0.25), (1.0, 0.0)]),
            ..UpdateModule::default()
        },
        render: lit_puff(0.02 * lift_reach, 0.6),
    });
    emitters.push(EmitterDef {
        // The base surge: a ring of dust running out along the ground, decelerating. A wide
        // speed spread fills it from the centre out (one speed draws a hard torus), a wide lift
        // spread makes the top ragged.
        name: "surge".into(),
        delay: 0.03 * tau,
        duration: None,
        spawn: burst(tier.pick(240, 90), (6.0 * tau, 9.0 * tau)),
        init: init(
            disc(70.0, (0.02 * big_r, 0.03 * big_r)),
            planar((1.2 * u, 2.6 * u), (0.03 * u, 0.15 * u)),
            tier.pick((0.22 * big_r, 0.32 * big_r), (0.3 * big_r, 0.42 * big_r)),
            dust,
        ),
        update: UpdateModule {
            gravity: drift(medium, 0.015 * G, surge_drag),
            drag: surge_drag,
            curl: Some((1.6, 0.05 / big_r.sqrt(), 0.25)),
            size_over_life: keys(&[(0.0, 0.6), (1.0 / 7.0, 1.3), (3.0 / 7.0, 2.0), (1.0, 2.6)]),
            alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.55), (4.0 / 7.0, 0.4), (1.0, 0.0)]),
            ..UpdateModule::default()
        },
        render: lit_puff(0.04 * big_r, 0.7),
    });
    if tier == CrashTier::Full {
        emitters.push(EmitterDef {
            // Slower, taller dust filling the surge's inside.
            name: "surge_billow".into(),
            delay: 0.05 * tau,
            duration: None,
            spawn: burst(60, (7.0 * tau, 10.0 * tau)),
            init: init(
                disc(70.0, (0.01 * big_r, 0.02 * big_r)),
                planar((0.4 * u, 1.0 * u), (0.1 * u, 0.3 * u)),
                (0.3 * big_r, 0.45 * big_r),
                dust,
            ),
            update: UpdateModule {
                gravity: drift(medium, 0.02 * G, 0.5 / tau),
                drag: 0.5 / tau,
                curl: Some((0.8, 0.05 / big_r.sqrt(), 0.2)),
                color_over_life: ramp(&[(0.0, "#d8d8d8"), (1.0, "#ffffff")]),
                size_over_life: keys(&[(0.0, 0.5), (2.0 / 7.0, 1.3), (1.0, 2.2)]),
                alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.6), (4.0 / 7.0, 0.45), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: lit_puff(0.06 * big_r, 0.75),
        });
    }
    if ground == GroundClass::Snow {
        let (_, earth, _) = ground.colors();
        emitters.push(EmitterDef {
            // Frozen earth the blast tears up under the snow, lifted with the powder: a few
            // darker puffs mixed into the pale surge (ADR-1425), low and short-lived since
            // soil grains are heavier than snow.
            name: "lifted_earth".into(),
            delay: 0.02 * tau,
            duration: None,
            spawn: burst(tier.pick(70, 24), (2.0 * tau, 3.5 * tau)),
            init: init(
                disc(70.0, (0.02 * big_r, 0.05 * big_r)),
                planar((0.8 * u, 2.0 * u), (0.02 * u, 0.08 * u)),
                tier.pick((0.12 * big_r, 0.2 * big_r), (0.18 * big_r, 0.26 * big_r)),
                earth,
            ),
            update: UpdateModule {
                gravity: drift(medium, -0.3 * lift_drag, surge_drag),
                drag: surge_drag,
                curl: Some((1.4, 0.05 / big_r.sqrt(), 0.25)),
                size_over_life: keys(&[(0.0, 0.6), (2.0 / 7.0, 1.3), (1.0, 2.0)]),
                alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.5), (4.0 / 7.0, 0.3), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: lit_puff(0.04 * big_r, 0.7),
        });
    }
    emitters.extend(dust_wall(ground, scale, tier, medium));
    // Snow powder is a thinner cloud than soil dust ([`GroundClass::dust_opacity`]): the lifted
    // cover and the surge thin out. The front's skirt and the dust wall keep their opacity (the
    // X2 front checks; the wall is what shows the front from kilometres away).
    let opacity = ground.dust_opacity();
    for emitter in emitters.iter_mut().filter(|e| {
        e.name != "lifted_earth" && e.name != "blast_front" && !e.name.starts_with(WALL_EMITTER_PREFIX)
    }) {
        for key in &mut emitter.update.alpha_over_life {
            key.v *= opacity;
        }
    }
    system(
        ground.dust_effect(scale),
        6_144,
        lod(scale, 1_500.0, 4_000.0),
        emitters,
    )
}

/// The latest a wall ring may leave the crash point before the front reaches the bottom of its
/// band `r_low`, seconds, and still never lead the front: a puff launched at `k r` reaches
/// `r (1 − e^(−kt))`, and the front `343 (t − L)` stays ahead of it while
/// `L ≤ (ln(k r / 343) + 1) / k` (0.8 of that, at most 30 ms).
fn wall_lead(r_low: f32) -> f32 {
    let x = WALL_DRAG * r_low / SHOCK_FRONT_SPEED;
    if x <= 1.0 {
        return 0.0;
    }
    (0.8 * (x.ln() + 1.0) / WALL_DRAG).clamp(0.0, 0.03)
}

/// The dust wall (`front_wall_<k>`, ADR-1497): the snow and dust the front kicks up along its
/// path, a ring of puffs per [`CrashScale::wall_rings`] radius. Ring `k` leaves the crash point
/// [`wall_lead`] before the front reaches it, comes to rest between it and the ring inside it
/// within two frames (`WALL_DRAG`) and rises from there, so the wall grows outward at the speed
/// of sound (its edge one ring ratio behind the front at most, never ahead) out to
/// [`CrashScale::wall_reach`], thinner and lower with distance as the overpressure falls.
fn dust_wall(ground: GroundClass, scale: CrashScale, tier: CrashTier, medium: &VfxMedium) -> Vec<EmitterDef> {
    let (dust, _, _) = ground.colors();
    let cbrt = scale.cbrt_kg();
    let tau = scale.time_scale();
    let u = scale.speed_scale();
    let rings = scale.wall_rings(tier);
    let ratio = tier.pick(WALL_RING_RATIO, WALL_RING_RATIO_LITE);
    let size = tier.pick((0.16 * cbrt, 0.28 * cbrt), (0.24 * cbrt, 0.38 * cbrt));
    // Puffs per metre of ring: about one mean puff size apart on Full, two on Lite.
    let per_metre = std::f32::consts::TAU / tier.pick(0.22 * cbrt, 0.62 * cbrt);
    let inner = rings.first().copied().unwrap_or(1.0);
    rings
        .iter()
        .enumerate()
        .map(|(k, &rk)| {
            let r_low = rk / ratio;
            // Overpressure (and the dust it lifts) falls about as 1/r here: the wall thins and
            // lowers outward.
            let fade = (inner / rk).sqrt();
            let alpha = tier.pick(0.6, 0.65) * fade;
            let rise = 0.15 * u * fade;
            EmitterDef {
                name: format!("{WALL_EMITTER_PREFIX}{k}"),
                delay: (rk / SHOCK_FRONT_SPEED - wall_lead(r_low)).max(0.0),
                duration: None,
                spawn: burst(((per_metre * rk).round() as u32).max(6), (1.5 * tau, 2.4 * tau)),
                init: init(
                    // On the axis at 0.5 m to a third of a puff up: the puffs leave the crash
                    // point in every direction along the ground and keep that height.
                    disc(0.0, (0.5, (0.33 * size.0).max(0.6))),
                    planar((WALL_DRAG * r_low, WALL_DRAG * rk), (0.0, 0.0)),
                    size,
                    dust,
                ),
                update: UpdateModule {
                    gravity: drift(medium, WALL_DRAG * rise, WALL_DRAG),
                    drag: WALL_DRAG,
                    size_over_life: keys(&[(0.0, 0.5), (0.1, 1.0), (1.0, 2.2)]),
                    // Full opacity from birth: curves are baked into `CURVE_SAMPLES` (8) points,
                    // so a fade-in would last a seventh of the life (0.7-1.1 s) and hide the
                    // newest ring. A puff is 80 % of the way to its ring, still behind the
                    // front, after its first step.
                    alpha_over_life: keys(&[
                        (0.0, alpha),
                        (0.25, 0.8 * alpha),
                        (0.6, 0.45 * alpha),
                        (1.0, 0.0),
                    ]),
                    ..UpdateModule::default()
                },
                render: lit_puff(0.02 * scale.lift_reach(), 0.6),
            }
        })
        .collect()
}

/// The gust puff (`crash_gust_snow`, ADR-1497): the loose snow a tree crown or a bank sheds as
/// the blast front passes, a short burst of small white puffs blown downwind of the front that
/// sink and fade within 2–3 s. Scale-free (sized for a 10–20 m crown); [`CrashSequence`] fires
/// one at each of its `gust_sites` on the frame the front reaches it, with `forward` along the
/// blast wind tilted up, so the powder leaves the crown along the gust.
#[must_use]
pub fn crash_gust_snow(tier: CrashTier, medium: &VfxMedium) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: CRASH_GUST_SNOW.into(),
        system: SystemModule {
            bounds: None,
            budget: 64,
            // As the contact puff: a few small puffs, full detail across the showcase valley.
            lod: Some(LodDef {
                distances: vec![4_000.0, 9_000.0],
                rate_scale: vec![1.0, 0.6, 0.0],
            }),
        },
        emitters: vec![EmitterDef {
            name: "powder".into(),
            delay: 0.0,
            duration: None,
            spawn: burst(tier.pick(10, 4), (1.8, 2.8)),
            init: init(
                ShapeDef::Sphere {
                    radius: r(1.0, 4.0),
                    surface: false,
                },
                upward(25.0, (3.0, 9.0)),
                tier.pick((1.5, 3.0), (2.5, 4.0)),
                "#e4e7ea",
            ),
            update: UpdateModule {
                // Snow powder settles at about 1 m/s (Locatelli & Hobbs 1974) and drifts.
                gravity: drift(medium, -0.8 * 1.5, 1.5),
                drag: 1.5,
                curl: Some((0.6, 0.15, 0.3)),
                size_over_life: keys(&[(0.0, 0.4), (0.15, 1.0), (1.0, 2.4)]),
                alpha_over_life: keys(&[(0.0, 0.0), (0.06, 0.55), (0.5, 0.3), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: lit_puff(0.6, 0.4),
        }],
    }
}

/// The ejecta for `ground` (`crash_ejecta_<ground>@<scale>`): a dark fountain of earth thrown
/// up in the first seconds (`fountain`, dust-laden puffs that slow by drag and hang) and clods
/// and rocks on ballistic arcs (`clods`: `1.2–2.6 √(gR)`, 45 to 90 degrees up: 200 to 400 m up
/// and landing 500 to 800 m out at 250 t), plus slow fines (Full). No flame trails, no glow.
/// Spawn on the ground point with `forward` on the normal.
#[must_use]
pub fn crash_ejecta(
    ground: GroundClass,
    scale: CrashScale,
    tier: CrashTier,
    medium: &VfxMedium,
) -> VfxSystemDef {
    let (_, clod, earth) = ground.colors();
    let big_r = scale.fireball_radius();
    let tau = scale.time_scale();
    let u = scale.speed_scale();
    let fountain_drag = 0.12 / tau;
    let launch = scale.ejecta_launch();
    let mut emitters = vec![
        EmitterDef {
            name: "fountain".into(),
            delay: 0.0,
            duration: None,
            spawn: burst(tier.pick(160, 60), (2.5 * tau, 3.5 * tau)),
            init: init(
                disc(35.0, (0.05 * big_r, 0.1 * big_r)),
                upward(35.0, (1.1 * u, 2.4 * u)),
                tier.pick((0.1 * big_r, 0.17 * big_r), (0.14 * big_r, 0.22 * big_r)),
                earth,
            ),
            update: UpdateModule {
                gravity: drift(medium, -medium.gravity, fountain_drag),
                drag: fountain_drag,
                curl: Some((1.2, 0.05 / big_r.sqrt(), 0.25)),
                color_over_life: ramp(&[(0.0, "#ffffff"), (1.0, "#d6d6d6")]),
                size_over_life: keys(&[(0.0, 0.5), (2.0 / 7.0, 1.4), (1.0, 3.0)]),
                alpha_over_life: keys(&[(0.0, 0.9), (3.0 / 7.0, 0.75), (5.0 / 7.0, 0.35), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: lit_puff(0.03 * big_r, 0.8),
        },
        EmitterDef {
            // Ballistic earth and rock: they land and rest (the debris lane's meshes take over).
            name: "clods".into(),
            delay: 0.0,
            duration: None,
            spawn: burst(tier.pick(700, 220), (6.0 * tau, 8.0 * tau)),
            init: InitModule {
                spin: r(-6.0, 6.0),
                ..init(
                    disc(50.0, (0.05 * big_r, 0.1 * big_r)),
                    upward(launch.cone_degrees, (launch.min_speed, launch.max_speed)),
                    (0.004 * big_r, 0.016 * big_r),
                    clod,
                )
            },
            update: UpdateModule {
                gravity: (0.0, -medium.gravity, 0.0),
                drag: 0.005,
                depth_collision: settle(0.05),
                alpha_over_life: keys(&[(0.0, 1.0), (6.0 / 7.0, 1.0), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: clod_mesh(),
        },
    ];
    if tier == CrashTier::Full {
        emitters.push(EmitterDef {
            // The fine fraction: fast at first, slowed by drag within seconds, then it hangs.
            name: "fines".into(),
            delay: 0.0,
            duration: None,
            spawn: burst(80, (4.0 * tau, 6.0 * tau)),
            init: init(
                disc(40.0, (0.05 * big_r, 0.1 * big_r)),
                upward(45.0, (0.8 * u, 1.6 * u)),
                (0.12 * big_r, 0.2 * big_r),
                earth,
            ),
            update: UpdateModule {
                gravity: drift(medium, -0.05 * G, 0.6 / tau),
                drag: 0.6 / tau,
                curl: Some((1.0, 0.05 / big_r.sqrt(), 0.2)),
                color_over_life: ramp(&[(0.0, "#ffffff"), (1.0, "#e0e0e0")]),
                size_over_life: keys(&[(0.0, 0.5), (2.0 / 7.0, 1.5), (1.0, 3.2)]),
                alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.5), (4.0 / 7.0, 0.3), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: lit_puff(0.05 * big_r, 0.75),
        });
    }
    system(
        ground.ejecta_effect(scale),
        2_048,
        lod(scale, 1_500.0, 4_000.0),
        emitters,
    )
}

/// The mushroom column (`crash_column@<scale>`):
///
/// - `cap`: the buoyant thermal the fireball becomes. It is launched at
///   [`CrashScale::column_height`] ÷ 30 s and decelerates with a 30 s time constant, so it
///   rises at about 25 m/s at first and stops near the column height (800 to 1500 m by 60 s at
///   250 t, 400 to 800 m across); it stays for more than 3 minutes and drifts with the wind.
///   It is grey-white condensation, snow powder and dust (`CAP_ALBEDO`, ADR-1472), with
///   `cap_veil` (a few large thin puffs that join the body into one mass) and `cap_wisps` (small
///   fast puffs that tear the rim) launched with it.
/// - `stem`: dirty snow and dust drawn up from the crater into the cap for 3 minutes, dark at
///   the crater and lighter with height.
/// - `crater_smoke` and `wreck_fire`: dense smoke rolling off the crater and the burning
///   hardware in it (sized for the body, not the yield).
///
/// The cap's puffs live 200 to 260 s (150 to 180 s on Lite). The GPU pool is a ring buffer, so
/// a puff survives only while fewer than the pool's capacity particles spawn after it: the whole
/// crash keeps its spawn rate low ([`crash_ring_slots`]).
///
/// Spawn on the ground point with `forward` on the normal.
#[must_use]
pub fn crash_column(scale: CrashScale, tier: CrashTier, medium: &VfxMedium) -> VfxSystemDef {
    let big_r = scale.fireball_radius();
    let tau = scale.time_scale();
    let height = scale.column_height();
    let cap_drag = 1.0 / 30.0;
    let cap_speed = height * cap_drag;
    let stem_rise = 0.55 * cap_speed;
    let stem_drag = 0.08;
    let cap_life = tier.pick((200.0, 260.0), (150.0, 180.0));
    let cap_update = |curl: f32, size: &[(f32, f32)], alpha: f32| UpdateModule {
        gravity: drift(medium, 0.0, cap_drag),
        drag: cap_drag,
        curl: Some((curl, 0.03 / big_r.sqrt(), 0.15)),
        color_over_life: ramp(&CAP_ALBEDO),
        size_over_life: keys(&cap_aged_keys(size, CAP_AGE_DIFFUSION, true)),
        // Near its full opacity within 7 s: the young cap is its densest, and a cap that fades
        // in over half a minute shows the sky between its puffs for the whole shot.
        alpha_over_life: keys(&cap_aged_keys(
            &[
                (0.0, 0.0),
                (0.03, 0.75 * alpha),
                (CAP_YOUNG_UNTIL, alpha),
                (4.0 / 7.0, 0.75 * alpha),
                (6.0 / 7.0, 0.375 * alpha),
                (1.0, 0.0),
            ],
            CAP_AGE_DIFFUSION,
            false,
        )),
        ..UpdateModule::default()
    };
    system(
        column_effect(scale),
        1_024,
        // A column reads from far away: full detail to 2 km × the LOD factor, thinner past it.
        lod(scale, 2_000.0, 12_000.0),
        vec![
            EmitterDef {
                // The body of the cap: puffs of a wide spread of sizes and launch speeds, so the
                // mass is lumpy at several scales and not one layer of equal round puffs.
                name: "cap".into(),
                delay: 0.15 * tau,
                duration: None,
                spawn: burst(tier.pick(70, 30), cap_life),
                init: init(
                    disc(60.0, (0.25 * big_r, 0.45 * big_r)),
                    upward(12.0, (0.85 * cap_speed, 1.15 * cap_speed)),
                    tier.pick((0.4 * big_r, 0.85 * big_r), (0.55 * big_r, 0.95 * big_r)),
                    "#ffffff",
                ),
                update: cap_update(
                    1.3,
                    &[
                        (0.0, 0.5),
                        (0.04, 1.2),
                        (1.0 / 7.0, 1.6),
                        (3.0 / 7.0, 2.6),
                        (1.0, 3.4),
                    ],
                    0.8,
                ),
                // Less packed than a soot cap (0.88): bright condensation carries light deep
                // into the mass, so a puff's core is not a dark hole between sunlit rinds.
                render: lit_puff(0.15 * big_r, 0.72),
            },
            EmitterDef {
                // A few large thin puffs riding with the cap's centre: they fill the gaps between
                // the body's puffs, so the cap reads as one continuous mass (ADR-1472).
                name: "cap_veil".into(),
                delay: 0.2 * tau,
                duration: None,
                spawn: burst(tier.pick(14, 6), cap_life),
                init: init(
                    disc(45.0, (0.3 * big_r, 0.4 * big_r)),
                    upward(8.0, (0.9 * cap_speed, 1.05 * cap_speed)),
                    (0.9 * big_r, 1.2 * big_r),
                    "#ffffff",
                ),
                update: cap_update(
                    1.0,
                    &[
                        (0.0, 0.5),
                        (0.04, 1.15),
                        (1.0 / 7.0, 1.5),
                        (3.0 / 7.0, 2.3),
                        (1.0, 2.9),
                    ],
                    0.35,
                ),
                render: lit_puff(0.25 * big_r, 0.5),
            },
            EmitterDef {
                // Small puffs in the body's own speed band with more turbulence, growing faster
                // than the body: a ragged, torn rim instead of a clean outline of round puffs
                // (thrown out faster and wider, they flew off as separate balls).
                name: "cap_wisps".into(),
                delay: 0.15 * tau,
                duration: None,
                spawn: burst(tier.pick(26, 8), cap_life),
                init: init(
                    disc(60.0, (0.3 * big_r, 0.45 * big_r)),
                    upward(12.0, (0.85 * cap_speed, 1.1 * cap_speed)),
                    (0.2 * big_r, 0.4 * big_r),
                    "#ffffff",
                ),
                update: cap_update(
                    2.6,
                    &[
                        (0.0, 0.5),
                        (0.04, 1.6),
                        (1.0 / 7.0, 2.4),
                        (3.0 / 7.0, 3.8),
                        (1.0, 4.8),
                    ],
                    0.4,
                ),
                render: lit_puff(0.1 * big_r, 0.4),
            },
            EmitterDef {
                // Dirty snow and dust drawn up from the crater into the cap, thinning and
                // lightening with height (`STEM_ALBEDO`).
                name: "stem".into(),
                delay: 0.3 * tau,
                duration: Some(180.0),
                spawn: rate(
                    tier.pick(3.0, 1.2),
                    (0.85 * height / stem_rise, height / stem_rise),
                ),
                init: init(
                    disc(40.0, (0.02 * big_r, 0.05 * big_r)),
                    upward(8.0, (0.4 * cap_speed, 0.6 * cap_speed)),
                    tier.pick((0.4 * big_r, 0.9 * big_r), (0.55 * big_r, 0.95 * big_r)),
                    "#ffffff",
                ),
                update: UpdateModule {
                    gravity: drift(medium, stem_rise * stem_drag, stem_drag),
                    drag: stem_drag,
                    curl: Some((2.0, 0.03 / big_r.sqrt(), 0.15)),
                    color_over_life: ramp(&STEM_ALBEDO),
                    size_over_life: keys(&[(0.0, 0.6), (3.0 / 7.0, 1.4), (1.0, 2.0)]),
                    alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.75), (5.0 / 7.0, 0.5), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: lit_puff(0.1 * big_r, 0.7),
            },
            EmitterDef {
                // Dense, near-black smoke rolling off the crater and the burning hardware.
                name: "crater_smoke".into(),
                delay: 0.5 * tau,
                duration: None,
                spawn: rate(tier.pick(2.5, 1.5), (8.0, 12.0)),
                init: init(
                    disc(50.0, (0.5, 1.5)),
                    planar((0.5, 2.0), (1.0, 2.5)),
                    tier.pick((6.0, 9.0), (8.0, 11.0)),
                    "#2c2c2c",
                ),
                update: UpdateModule {
                    gravity: drift(medium, 1.2, 0.6),
                    drag: 0.6,
                    curl: Some((0.8, 0.08, 0.2)),
                    size_over_life: keys(&[(0.0, 0.6), (1.0, 2.2)]),
                    alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.9), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: lit_puff(2.5, 0.85),
            },
            EmitterDef {
                // Flames on the hardware in the crater: the flame sheet carries the hue.
                name: "wreck_fire".into(),
                delay: 0.5 * tau,
                duration: None,
                spawn: rate(tier.pick(6.0, 3.0), (1.0, 1.6)),
                init: InitModule {
                    rotation: r(-0.25, 0.25),
                    spin: r(-0.2, 0.2),
                    ..init(
                        disc(40.0, (0.3, 1.0)),
                        upward(15.0, (2.0, 4.0)),
                        (2.5, 4.5),
                        "#ffffff",
                    )
                },
                update: UpdateModule {
                    gravity: (0.0, 2.0, 0.0),
                    drag: 1.0,
                    color_over_life: ramp(&[(0.0, "#8e8e8e"), (1.0, "#4a4a4a")]),
                    size_over_life: keys(&[(0.0, 0.6), (0.3, 1.0), (1.0, 0.5)]),
                    alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 1.0), (5.0 / 7.0, 0.8), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::FireAnim,
                    flipbook: Some(flame_sheet()),
                    blend: BlendMode::Alpha,
                    softness: 0.8,
                    emissive: 4.0,
                    light: Some(ParticleLightDef {
                        color: "#ff9a40".into(),
                        intensity: 12.0,
                        radius: 30.0,
                    }),
                    ..RenderModule::default()
                },
            },
        ],
    )
}

/// The first-contact puff for `ground` (`crash_contact_<ground>`): a short radial puff and a
/// few clods where a 12–14 m body first touches the ground, a few frames before the blast
/// ([`CrashSequence`]). Sized for the body (use the instance scale for other bodies). Spawn on
/// the ground point with `forward` on the normal.
#[must_use]
pub fn crash_contact(ground: GroundClass, tier: CrashTier, medium: &VfxMedium) -> VfxSystemDef {
    let (dust, clod, _) = ground.colors();
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: ground.contact_effect().into(),
        system: SystemModule {
            bounds: None,
            budget: 128,
            lod: Some(LodDef {
                distances: vec![4_000.0, 9_000.0],
                rate_scale: vec![1.0, 0.6, 0.0],
            }),
        },
        emitters: vec![
            EmitterDef {
                name: "puff".into(),
                delay: 0.0,
                duration: None,
                spawn: burst(tier.pick(36, 14), (0.8, 1.4)),
                init: init(
                    disc(80.0, (0.3, 0.5)),
                    planar((14.0, 32.0), (1.0, 4.0)),
                    tier.pick((4.0, 7.0), (6.0, 9.0)),
                    dust,
                ),
                update: UpdateModule {
                    gravity: drift(medium, 0.2, 2.5),
                    drag: 2.5,
                    curl: Some((1.0, 0.1, 0.2)),
                    size_over_life: keys(&[(0.0, 0.6), (1.0 / 7.0, 1.2), (1.0, 2.4)]),
                    alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.7), (4.0 / 7.0, 0.4), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: lit_puff(1.0, 0.7),
            },
            EmitterDef {
                name: "clods".into(),
                delay: 0.0,
                duration: None,
                spawn: burst(tier.pick(10, 5), (2.5, 4.0)),
                init: InitModule {
                    spin: r(-8.0, 8.0),
                    ..init(
                        disc(60.0, (0.3, 0.6)),
                        upward(55.0, (6.0, 16.0)),
                        (0.2, 0.6),
                        clod,
                    )
                },
                update: UpdateModule {
                    gravity: (0.0, -medium.gravity, 0.0),
                    drag: 0.1,
                    depth_collision: settle(0.05),
                    alpha_over_life: keys(&[(0.0, 1.0), (0.8, 1.0), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: clod_mesh(),
            },
        ],
    }
}

/// A ground fire (`crash_ground_fire_<size>`): flames with a 30 m light (it lights the snow
/// around it) and a thin sooty plume, burning for [`FireSize::burn_seconds`] (at least 3
/// minutes). Spawn on the ground with `forward` on the normal.
#[must_use]
pub fn crash_ground_fire(size: FireSize, tier: CrashTier, medium: &VfxMedium) -> VfxSystemDef {
    let (flame, flame_rate, smoke_rate, light) = size.look();
    let burn = size.burn_seconds();
    let k = flame.1 / 2.2;
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: size.effect().into(),
        system: SystemModule {
            bounds: None,
            budget: 256,
            lod: Some(LodDef {
                distances: vec![4_000.0, 9_000.0],
                rate_scale: vec![1.0, 0.6, 0.0],
            }),
        },
        emitters: vec![
            EmitterDef {
                name: "flames".into(),
                delay: 0.0,
                duration: Some(burn),
                spawn: rate(tier.pick(flame_rate, 0.3 * flame_rate), (1.0, 1.6)),
                init: InitModule {
                    rotation: r(-0.25, 0.25),
                    spin: r(-0.2, 0.2),
                    ..init(
                        disc(40.0, (0.2 * k, 0.6 * k)),
                        upward(15.0, (1.5, 3.0)),
                        flame,
                        "#ffffff",
                    )
                },
                update: UpdateModule {
                    gravity: (0.0, 2.0, 0.0),
                    drag: 1.0,
                    color_over_life: ramp(&[(0.0, "#8e8e8e"), (1.0, "#4a4a4a")]),
                    size_over_life: keys(&[(0.0, 0.6), (0.3, 1.0), (1.0, 0.5)]),
                    alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 1.0), (5.0 / 7.0, 0.8), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::FireAnim,
                    flipbook: Some(flame_sheet()),
                    blend: BlendMode::Alpha,
                    softness: 0.5,
                    emissive: 4.0,
                    light: Some(ParticleLightDef {
                        color: "#ff9a40".into(),
                        intensity: light,
                        radius: 30.0,
                    }),
                    ..RenderModule::default()
                },
            },
            EmitterDef {
                name: "smoke".into(),
                delay: 0.3,
                duration: Some(burn),
                spawn: rate(tier.pick(smoke_rate, 0.25 * smoke_rate), (14.0, 20.0)),
                init: init(
                    disc(30.0, (0.5 * k, 1.0 * k)),
                    upward(10.0, (1.5, 2.5)),
                    (2.5 * k, 4.0 * k),
                    "#8a857e",
                ),
                update: UpdateModule {
                    gravity: drift(medium, 0.75, 0.25),
                    drag: 0.25,
                    curl: Some((1.5, 0.05, 0.2)),
                    // Wet March vegetation burns to a pale grey smoke.
                    color_over_life: ramp(&[(0.0, "#b0b0b0"), (1.0, "#e8e8e8")]),
                    size_over_life: keys(&[(0.0, 0.5), (2.0 / 7.0, 1.8), (1.0, 5.0)]),
                    alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 0.6), (4.0 / 7.0, 0.35), (1.0, 0.0)]),
                    ..UpdateModule::default()
                },
                render: lit_puff(1.5, 0.8),
            },
        ],
    }
}

/// Particle-pool slots a whole crash at `scale` (every layer on snow, the shell, the contact
/// puff, its [`CrashScale::fire_count`] fires and the most gust puffs a sequence fires)
/// occupies over its longest particle lifetime: every burst plus every rate emitter's spawns over that time. The GPU pool is a ring buffer
/// of `VfxProfile::particle_capacity` slots, so a long-lived puff is overwritten once that many
/// particles spawn after it: keep this under the capacity, with room for the scene's other
/// effects (about 35 000 on Full and 8 000 on Lite at 250 t).
#[must_use]
pub fn crash_ring_slots(scale: CrashScale, tier: CrashTier, medium: &VfxMedium) -> u32 {
    let ground = GroundClass::Snow;
    let mut defs: Vec<(VfxSystemDef, u32)> = vec![
        (crash_fireball(scale, tier, medium), 1),
        (crash_shell(scale, tier), 1),
        (crash_dust(ground, scale, tier, medium), 1),
        (crash_ejecta(ground, scale, tier, medium), 1),
        (crash_column(scale, tier, medium), 1),
        (crash_contact(ground, tier, medium), 1),
        // Every gust puff a sequence may fire (ADR-1497).
        (
            crash_gust_snow(tier, medium),
            CrashSequence::MAX_GUST_PUFFS as u32,
        ),
    ];
    let sites = crash_fire_sites(scale, CrashBurst::default().seed);
    for size in FireSize::ALL {
        let n = sites.iter().filter(|s| s.size == size).count() as u32;
        defs.push((crash_ground_fire(size, tier, medium), n));
    }
    let longest = defs
        .iter()
        .flat_map(|(d, _)| d.emitters.iter())
        .map(|e| e.delay + e.spawn.lifetime.max)
        .fold(0.0f32, f32::max);
    let mut slots = 0.0f32;
    for (def, copies) in &defs {
        for e in &def.emitters {
            let n = match e.spawn.mode {
                SpawnMode::Burst(n) => n as f32,
                SpawnMode::Rate(r) => r * e.duration.unwrap_or(longest).min(longest),
                SpawnMode::PerDistance { .. } | SpawnMode::Event => 0.0,
            };
            slots += n * *copies as f32;
        }
    }
    slots.ceil() as u32
}

/// Every crash effect for `scale` and `tier` in `medium`: the fireball, shell and column at the
/// scale, a blast-and-surge and an ejecta effect per [`GroundClass`] at the scale, and the
/// scale-free contact puffs, ground fires and gust puff ([`CRASH_GUST_SNOW`]).
#[must_use]
pub fn crash_effects(scale: CrashScale, tier: CrashTier, medium: &VfxMedium) -> Vec<VfxSystemDef> {
    let mut out = vec![
        crash_fireball(scale, tier, medium),
        crash_shell(scale, tier),
        crash_column(scale, tier, medium),
    ];
    for ground in GroundClass::ALL {
        out.push(crash_dust(ground, scale, tier, medium));
        out.push(crash_ejecta(ground, scale, tier, medium));
        out.push(crash_contact(ground, tier, medium));
    }
    for size in FireSize::ALL {
        out.push(crash_ground_fire(size, tier, medium));
    }
    out.push(crash_gust_snow(tier, medium));
    out
}

/// The crash effect called `name` for `tier` in `medium` (a scaled name carries its scale:
/// `crash_fireball@250t`), or `None` when `name` is not one.
#[must_use]
pub fn crash_effect(name: &str, tier: CrashTier, medium: &VfxMedium) -> Option<VfxSystemDef> {
    if let Some(size) = FireSize::ALL.into_iter().find(|s| s.effect() == name) {
        return Some(crash_ground_fire(size, tier, medium));
    }
    if let Some(ground) = GroundClass::ALL.into_iter().find(|g| g.contact_effect() == name) {
        return Some(crash_contact(ground, tier, medium));
    }
    if name == CRASH_GUST_SNOW {
        return Some(crash_gust_snow(tier, medium));
    }
    let scale = CrashScale::from_effect_name(name)?;
    if name == fireball_effect(scale) {
        return Some(crash_fireball(scale, tier, medium));
    }
    if name == shell_effect(scale) {
        return Some(crash_shell(scale, tier));
    }
    if name == column_effect(scale) {
        return Some(crash_column(scale, tier, medium));
    }
    GroundClass::ALL.into_iter().find_map(|ground| {
        if name == ground.dust_effect(scale) {
            Some(crash_dust(ground, scale, tier, medium))
        } else if name == ground.ejecta_effect(scale) {
            Some(crash_ejecta(ground, scale, tier, medium))
        } else {
            None
        }
    })
}

/// Registers (or refreshes) every crash effect for `scale` and `tier` in `scene` with `wind`
/// (`None`: the scene medium's wind). A game's own definition of a crash name is replaced, so
/// call this only when the built-in look is wanted. Returns how many definitions changed.
pub fn ensure_crash_effects(
    scene: &mut VfxScene,
    scale: CrashScale,
    tier: CrashTier,
    wind: Option<Vec3>,
) -> usize {
    let mut medium = scene.medium();
    if let Some(w) = wind.filter(|w| w.is_finite()) {
        medium.wind = w;
    }
    let mut changed = 0;
    for def in crash_effects(scale, tier, &medium) {
        let same = scene
            .library()
            .index(&def.name)
            .is_some_and(|i| *scene.library().def(i) == def);
        if same {
            continue;
        }
        let name = def.name.clone();
        match scene.library_mut().upsert(def) {
            Ok(_) => changed += 1,
            Err(err) => tracing::warn!(target: "regia_vfx", "crash effect {name} rejected: {err}"),
        }
    }
    changed
}

/// One crash at a point: its size, which layers to fire, and where.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct CrashBurst {
    /// Ground point of the crash (world).
    pub position: Vec3,
    /// Ground normal (local +Z of every layer).
    pub normal: Vec3,
    /// The loose ground (dust colour, clods, contact puff).
    pub ground: GroundClass,
    /// The size of the energy release.
    pub scale: CrashScale,
    /// Seed of the first layer (the others derive from it).
    pub seed: u64,
    /// Fire the fireball and its flash.
    pub fireball: bool,
    /// Fire the condensation shell (humid air only; off by default).
    pub shell: bool,
    /// Fire the blast ring, lifted cover and base surge.
    pub dust: bool,
    /// Fire the ejecta fountain and clods.
    pub ejecta: bool,
    /// Start the mushroom column (cap, stem, crater smoke).
    pub column: bool,
    /// Light the ground fires ([`crash_fire_sites`]).
    pub fires: bool,
    /// Fire the first-contact puff before the detonation ([`CrashSequence`] only).
    pub contact: bool,
    /// How long before the detonation the contact puff fires, seconds (3 frames at 60 Hz by
    /// default).
    pub contact_lead: f32,
    /// The falling body's unit travel direction (world) at contact, or `None` for a fall
    /// straight down (the default). The contact puff tilts downrange by half its angle from the
    /// vertical ([`contact_forward`], ADR-1497); a host with an approach model fills it from that.
    pub contact_downrange: Option<Vec3>,
    /// Fire a gust puff (`crash_gust_snow`) at each of the sequence's gust sites as the blast
    /// front reaches it ([`CrashSequence::gust_sites`]; on by default, nothing fires without
    /// sites).
    pub gust: bool,
}

impl Default for CrashBurst {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            normal: Vec3::Y,
            ground: GroundClass::Mud,
            scale: CrashScale::VEHICLE,
            seed: 0x0063_7261_7368,
            fireball: true,
            shell: false,
            dust: true,
            ejecta: true,
            column: true,
            fires: true,
            contact: true,
            contact_lead: 0.05,
            contact_downrange: None,
            gust: true,
        }
    }
}

/// One ground fire placed by [`crash_fire_sites`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FireSite {
    /// Offset from the crash point in its ground plane, metres: `[along tangent, along
    /// bitangent]` ([`ground_frame`]).
    pub offset: [f32; 2],
    /// Fire size.
    pub size: FireSize,
}

/// A tangent and bitangent of the ground plane of `normal` (for `+Y`: `+X` and `+Z`).
#[must_use]
pub fn ground_frame(normal: Vec3) -> (Vec3, Vec3) {
    let n = normal.normalize_or(Vec3::Y);
    let t = (Vec3::X - n * n.x).normalize_or(Vec3::Z);
    let b = t.cross(n);
    (t, b)
}

/// SplitMix64: a small, fixed, portable generator (the placements must not change with a
/// dependency's version).
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn unit(state: &mut u64) -> f32 {
    (splitmix(state) >> 40) as f32 / (1u64 << 24) as f32
}

/// The ground fires a crash of `scale` lights: [`CrashScale::fire_count`] sites, area-uniform
/// between 1.3 crater radii and [`CrashScale::fire_zone`], half small, a third medium, the rest
/// large. Deterministic in `seed`.
#[must_use]
pub fn crash_fire_sites(scale: CrashScale, seed: u64) -> Vec<FireSite> {
    let inner = 0.65 * scale.crater().diameter;
    let outer = scale.fire_zone().max(inner + 1.0);
    let mut state = seed ^ 0x6669_7265_7369_7465;
    (0..scale.fire_count())
        .map(|_| {
            let radius = (unit(&mut state) * (outer * outer - inner * inner) + inner * inner).sqrt();
            let angle = unit(&mut state) * std::f32::consts::TAU;
            let pick = unit(&mut state);
            let size = if pick < 0.5 {
                FireSize::Small
            } else if pick < 0.85 {
                FireSize::Medium
            } else {
                FireSize::Large
            };
            // det_sin_cos: the offset is the world position the fire placements hand to
            // scene.spawn, so it must not follow the platform libm
            // (fix-vfx1-determinism finding 6).
            let (sin, cos) = det_sin_cos(angle);
            FireSite {
                offset: [radius * cos, radius * sin],
                size,
            }
        })
        .collect()
}

/// One ground fire of a crash, placed on the ground: where the visible fire effect spawns and
/// the seed it spawns with. The fire lights ([`crate::fire_light::FireLights::spawn_crash_sites`])
/// use the same positions and seeds, so the lit area follows the visible fires.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FirePlacement {
    /// Fire size.
    pub size: FireSize,
    /// World position of the fire.
    pub position: Vec3,
    /// Seed of the fire's effect instance.
    pub seed: u64,
}

/// The ground fires of `crash` ([`crash_fire_sites`]) with their world positions and seeds, as
/// [`spawn_crash_on`] spawns them; empty when `crash.fires` is off. `ground` maps a point of the
/// crash's ground plane to the terrain point there (`None` skips that fire).
#[must_use]
pub fn crash_fire_placements(
    crash: &CrashBurst,
    ground: &dyn Fn(Vec3) -> Option<Vec3>,
) -> Vec<FirePlacement> {
    if !crash.fires {
        return Vec::new();
    }
    let normal = crash.normal.normalize_or(Vec3::Y);
    let (t, b) = ground_frame(normal);
    crash_fire_sites(crash.scale, crash.seed)
        .into_iter()
        .enumerate()
        .filter_map(|(i, site)| {
            let flat = crash.position + t * site.offset[0] + b * site.offset[1];
            Some(FirePlacement {
                size: site.size,
                position: ground(flat)?,
                seed: crash.seed.wrapping_add((100 + i as u64) * 0x9e37_79b9),
            })
        })
        .collect()
}

/// Fires the detonation layers of `crash` in `scene` (the effects must be registered for its
/// scale: [`ensure_crash_effects`]) on flat ground through the crash point. Returns `(effect
/// name, instance id)` for every layer and fire that spawned. The contact puff is not fired
/// here ([`spawn_crash_contact`], [`CrashSequence`]).
pub fn spawn_crash(scene: &mut VfxScene, crash: &CrashBurst) -> Vec<(String, u64)> {
    spawn_crash_on(scene, crash, &|p| Some(p))
}

/// [`spawn_crash`] with the ground fires placed by `ground`: it maps a point of the crash's
/// ground plane to the terrain point there (`None` skips that fire).
pub fn spawn_crash_on(
    scene: &mut VfxScene,
    crash: &CrashBurst,
    ground: &dyn Fn(Vec3) -> Option<Vec3>,
) -> Vec<(String, u64)> {
    let normal = crash.normal.normalize_or(Vec3::Y);
    let scale = crash.scale;
    // The fireball's lights sit in the ball, not in the ground (ADR-1498).
    let ball = crash.position + normal * (FIREBALL_SPAWN_HEIGHT * scale.fireball_radius());
    let layers = [
        (crash.fireball, fireball_effect(scale), ball),
        (crash.shell, shell_effect(scale), crash.position),
        (crash.dust, crash.ground.dust_effect(scale), crash.position),
        (crash.ejecta, crash.ground.ejecta_effect(scale), crash.position),
        (crash.column, column_effect(scale), crash.position),
    ];
    let mut out = Vec::new();
    for (i, (wanted, name, at)) in layers.into_iter().enumerate() {
        if !wanted {
            continue;
        }
        let seed = crash.seed.wrapping_add(i as u64 * 0x9e37_79b9);
        if let Some(id) = scene.spawn(&name, at, normal, seed) {
            out.push((name, id));
        }
    }
    for placement in crash_fire_placements(crash, ground) {
        let name = placement.size.effect();
        if let Some(id) = scene.spawn(name, placement.position, normal, placement.seed) {
            out.push((name.to_string(), id));
        }
    }
    out
}

/// Fires the first-contact puff of `crash` (registered by [`ensure_crash_effects`]).
pub fn spawn_crash_contact(scene: &mut VfxScene, crash: &CrashBurst) -> Option<(String, u64)> {
    let name = crash.ground.contact_effect();
    let seed = crash.seed ^ 0x0063_6f6e_7461_6374;
    scene
        .spawn(name, crash.position, contact_forward(crash), seed)
        .map(|id| (name.to_string(), id))
}

/// The contact puff's `forward`: the ground normal tilted downrange by half the body's angle
/// from the vertical ([`CrashBurst::contact_downrange`]; 10 degrees for a 70-degree fall), so a
/// slanting body splashes its puff and clods ahead of it. The normal itself without a travel
/// direction, for a vertical fall, or for a travel that does not head into the ground.
#[must_use]
pub fn contact_forward(crash: &CrashBurst) -> Vec3 {
    let normal = crash.normal.normalize_or(Vec3::Y);
    let Some(travel) = crash
        .contact_downrange
        .filter(|t| t.is_finite())
        .map(Vec3::normalize_or_zero)
    else {
        return normal;
    };
    let into = -travel.dot(normal);
    let flat = travel + normal * into;
    if into <= 0.0 || flat.length() < 1e-4 {
        return normal;
    }
    // The travel's angle from the inward normal, halved.
    let tilt = 0.5 * flat.length().atan2(into);
    (normal * tilt.cos() + flat.normalize() * tilt.sin()).normalize_or(normal)
}

/// Seconds the blast front takes from `origin` to `listener` ([`SHOCK_FRONT_SPEED`],
/// [`blast_arrival_seconds`]): when a camera there may shake and hear it.
#[must_use]
pub fn blast_arrival_time(origin: Vec3, listener: Vec3) -> f32 {
    blast_arrival_seconds(origin.distance(listener)) as f32
}

/// What happened in a [`CrashSequence`] this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CrashEventKind {
    /// The body first touched the ground (the contact puff fired).
    Contact,
    /// The core ruptured: the detonation layers fired.
    Detonation,
    /// The crater formed (sent with the detonation): its size, for the decal, terrain and
    /// wreck lanes (the battered hull belongs partly buried at the crater point).
    Crater(CraterSpec),
    /// The blast front reached the listener (the camera), `distance` metres away; a camera
    /// shake was requested with it.
    BlastArrived {
        /// Listener distance from the crash point, metres.
        distance: f32,
    },
}

/// A crash event from [`drive_crash_sequences`].
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct CrashEvent {
    /// The entity carrying the [`CrashSequence`].
    pub source: Entity,
    /// The crash point (world).
    pub position: Vec3,
    /// The ground normal.
    pub normal: Vec3,
    /// What happened.
    pub kind: CrashEventKind,
}

/// A crash on a clock: the contact puff `crash.contact_lead` seconds before the detonation, the
/// detonation `impact_in` seconds after the sequence first runs, and from then on the blast
/// front on one clock ([`blast_front_reached`], ADR-1497): its arrival at the camera, a gust
/// puff at every gust site it passes, and the wind it drags ([`BlastGusts`]).
/// [`drive_crash_sequences`] steps it (deterministic under a fixed step) and sends
/// [`CrashEvent`]s.
#[derive(Component, Clone, Debug, PartialEq, Reflect)]
#[reflect(Component, Default)]
pub struct CrashSequence {
    /// The crash.
    pub crash: CrashBurst,
    /// Seconds from the sequence's first frame to the detonation.
    pub impact_in: f32,
    /// Also spawn a fire light at every visible ground fire when the detonation fires (needs
    /// [`CrashBurst::fires`] and the `FireLights` resource). On by default.
    pub fire_lights: bool,
    /// Points where loose snow sheds as the front passes (tree crowns, banks; world): a gust puff
    /// (`crash_gust_snow`) fires at each on the frame the front reaches it, the nearest
    /// [`Self::MAX_GUST_PUFFS`] to the listener at most. Empty by default (a host fills it from
    /// its vegetation, or with [`crash_gust_sites`]).
    pub gust_sites: Vec<Vec3>,
    /// Seconds since the first frame (runtime state).
    #[reflect(ignore)]
    clock: f64,
    /// The clock on the detonation frame (runtime state).
    #[reflect(ignore)]
    detonated_at: Option<f64>,
    /// Seconds since the detonation the last frame ran its front on (runtime state).
    #[reflect(ignore)]
    frame_elapsed: Option<f64>,
    #[reflect(ignore)]
    contact_fired: bool,
    #[reflect(ignore)]
    arrived: bool,
    /// Gust sites whose puff is still to fire (runtime state; `false` for sites past the cap).
    #[reflect(ignore)]
    gust_pending: Vec<bool>,
    #[reflect(ignore)]
    gust_puffs: u32,
    #[reflect(ignore)]
    instances: Vec<(String, u64)>,
}

impl Default for CrashSequence {
    fn default() -> Self {
        Self::new(CrashBurst::default(), 0.05)
    }
}

impl CrashSequence {
    /// Most gust puffs one sequence fires (each holds a GPU emitter slot for its 2–3 s).
    pub const MAX_GUST_PUFFS: usize = 48;

    /// `crash` detonating `impact_in` seconds after the sequence's first frame.
    #[must_use]
    pub fn new(crash: CrashBurst, impact_in: f32) -> Self {
        Self {
            crash,
            impact_in: if impact_in.is_finite() {
                impact_in.max(0.0)
            } else {
                0.0
            },
            fire_lights: true,
            gust_sites: Vec::new(),
            clock: 0.0,
            detonated_at: None,
            frame_elapsed: None,
            contact_fired: false,
            arrived: false,
            gust_pending: Vec::new(),
            gust_puffs: 0,
            instances: Vec::new(),
        }
    }

    /// Seconds since the sequence's first frame.
    #[must_use]
    pub fn clock(&self) -> f32 {
        self.clock as f32
    }

    /// Whether the contact puff fired.
    #[must_use]
    pub fn contact_fired(&self) -> bool {
        self.contact_fired
    }

    /// Whether the detonation fired.
    #[must_use]
    pub fn detonated(&self) -> bool {
        self.detonated_at.is_some()
    }

    /// Whether the blast front reached the listener.
    #[must_use]
    pub fn arrived(&self) -> bool {
        self.arrived
    }

    /// Seconds since the detonation frame (`None` before it): the one clock of the front.
    #[must_use]
    pub fn since_detonation(&self) -> Option<f64> {
        self.detonated_at.map(|at| (self.clock - at).max(0.0))
    }

    /// Blast-front radius now, metres (0 before the detonation; [`blast_front_radius`]).
    #[must_use]
    pub fn front_radius(&self) -> f32 {
        self.since_detonation()
            .map_or(0.0, |t| blast_front_radius(t as f32))
    }

    /// How many gust puffs fired so far.
    #[must_use]
    pub fn gust_puffs(&self) -> u32 {
        self.gust_puffs
    }

    /// The front as a [`BlastGusts`] entry for `source`, on the clock the last
    /// [`advance_crash_sequence`] ran the front on (so its wind reaches a listener on the frame
    /// `BlastArrived` does); `dt` is that frame's step. `None` before the detonation or once the
    /// front is past [`BlastGusts::RETIRE_RADIUS`].
    #[must_use]
    pub fn active_front(&self, source: Entity, dt: f32) -> Option<ActiveFront> {
        let elapsed = self.frame_elapsed?;
        let front = ActiveFront {
            source,
            origin: self.crash.position,
            normal: self.crash.normal.normalize_or(Vec3::Y),
            scale: self.crash.scale,
            elapsed,
            prev_elapsed: (elapsed - f64::from(dt.max(0.0))).max(0.0),
        };
        (front.radius() <= BlastGusts::RETIRE_RADIUS).then_some(front)
    }

    /// `(effect name, instance id)` of every layer fired so far.
    #[must_use]
    pub fn instances(&self) -> &[(String, u64)] {
        &self.instances
    }
}

/// Clock slack for comparing a fixed-step clock against authored times, seconds.
const CLOCK_EPS: f64 = 1e-4;

/// One frame of a [`CrashSequence`] in `scene`, then the clock advances by `dt`: fires the
/// contact puff once
/// `impact_in − contact_lead` is reached, the detonation layers once `impact_in` is reached
/// (registering the crash effects for its scale and `tier` when the library lacks them), and
/// from the detonation frame on runs the blast front's one clock ([`blast_front_reached`]): a
/// gust puff at each gust site the front has reached, and the blast's arrival at `listener`.
/// Returns what happened this frame, in order.
pub fn advance_crash_sequence(
    scene: &mut VfxScene,
    seq: &mut CrashSequence,
    dt: f32,
    tier: CrashTier,
    listener: Option<Vec3>,
) -> Vec<CrashEventKind> {
    let mut events = Vec::new();
    let crash = seq.crash;
    let impact = f64::from(seq.impact_in);
    let needs = |scene: &VfxScene| scene.library().index(&fireball_effect(crash.scale)).is_none();
    let contact_at = impact - f64::from(crash.contact_lead.max(0.0));
    if !seq.contact_fired && seq.clock + CLOCK_EPS >= contact_at {
        seq.contact_fired = true;
        if crash.contact {
            if needs(scene) {
                ensure_crash_effects(scene, crash.scale, tier, None);
            }
            if let Some(fired) = spawn_crash_contact(scene, &crash) {
                seq.instances.push(fired);
            }
            events.push(CrashEventKind::Contact);
        }
    }
    if seq.detonated_at.is_none() && seq.clock + CLOCK_EPS >= impact {
        seq.detonated_at = Some(seq.clock);
        if needs(scene) {
            ensure_crash_effects(scene, crash.scale, tier, None);
        }
        seq.instances.extend(spawn_crash(scene, &crash));
        events.push(CrashEventKind::Detonation);
        events.push(CrashEventKind::Crater(crash.scale.crater()));
        seq.gust_pending = pick_gust_sites(&seq.gust_sites, crash.position, listener);
    }
    if let Some(elapsed) = seq.since_detonation() {
        seq.frame_elapsed = Some(elapsed);
        if crash.gust {
            fire_gust_puffs(scene, seq, elapsed);
        }
        if !seq.arrived
            && let Some(listener) = listener
        {
            let distance = crash.position.distance(listener);
            if blast_front_reached(elapsed, distance) {
                seq.arrived = true;
                events.push(CrashEventKind::BlastArrived { distance });
            }
        }
    }
    if dt.is_finite() && dt > 0.0 {
        seq.clock += f64::from(dt);
    }
    events
}

/// Which gust sites may fire (`false` marks a site left out): the nearest
/// [`CrashSequence::MAX_GUST_PUFFS`] to the listener (to the crash point without one), ties
/// broken by index, so the choice is deterministic.
fn pick_gust_sites(sites: &[Vec3], origin: Vec3, listener: Option<Vec3>) -> Vec<bool> {
    let focus = listener.unwrap_or(origin);
    let mut order: Vec<usize> = (0..sites.len()).filter(|&i| sites[i].is_finite()).collect();
    order.sort_by(|&a, &b| {
        sites[a]
            .distance_squared(focus)
            .total_cmp(&sites[b].distance_squared(focus))
            .then(a.cmp(&b))
    });
    // `true` = still to fire; sites past the cap start as already fired.
    let mut pending = vec![false; sites.len()];
    for &i in order.iter().take(CrashSequence::MAX_GUST_PUFFS) {
        pending[i] = true;
    }
    pending
}

/// Fires the gust puff of every pending site the front has reached ([`blast_front_reached`]),
/// with `forward` along the blast wind there tilted 40 degrees up.
fn fire_gust_puffs(scene: &mut VfxScene, seq: &mut CrashSequence, elapsed: f64) {
    let crash = seq.crash;
    let normal = crash.normal.normalize_or(Vec3::Y);
    for (i, pending) in seq.gust_pending.iter_mut().enumerate() {
        let Some(&site) = seq.gust_sites.get(i) else {
            break;
        };
        if !*pending || !blast_front_reached(elapsed, crash.position.distance(site)) {
            continue;
        }
        *pending = false;
        let rel = site - crash.position;
        let out = (rel - normal * rel.dot(normal)).normalize_or(Vec3::X);
        let forward =
            (out * 40f32.to_radians().cos() + normal * 40f32.to_radians().sin()).normalize_or(normal);
        let seed = crash.seed.wrapping_add((1_000 + i as u64) * 0x9e37_79b9);
        if let Some(id) = scene.spawn(CRASH_GUST_SNOW, site, forward, seed) {
            seq.gust_puffs += 1;
            seq.instances.push((CRASH_GUST_SNOW.to_string(), id));
        }
    }
}

/// Drives every [`CrashSequence`] ([`advance_crash_sequence`], the tier from the VFX profile,
/// the listener at the [`crate::VfxView`] camera): sends a [`CrashEvent`] for each step
/// and a [`CameraShakeRequest`] when the blast front reaches the camera
/// ([`CrashScale::blast_shake`]). With the detonation it adds the fire lights of a sequence
/// that asks for them (`CrashSequence::fire_lights`, ADR-1448). The two message writers are one
/// tuple parameter, which keeps the system within clippy's argument limit.
#[tracing::instrument(skip_all)]
pub fn drive_crash_sequences(
    time: Res<Time>,
    settings: Option<Res<crate::plugin::VfxSettings>>,
    view: Option<Res<crate::plugin::VfxView>>,
    scene: Option<ResMut<VfxScene>>,
    mut fire_lights: Option<ResMut<crate::fire_light::FireLights>>,
    mut sequences: Query<(Entity, &mut CrashSequence, Has<regia_scene::layers::LayerParked>)>,
    (mut events, mut shakes): (MessageWriter<CrashEvent>, MessageWriter<CameraShakeRequest>),
) {
    let _span = tracing::debug_span!("drive_crash_sequences").entered();
    let Some(mut scene) = scene else { return };
    let dt = time.delta_secs();
    let tier = settings
        .as_deref()
        .map_or(CrashTier::Full, |s| CrashTier::for_profile(&s.profile));
    let listener = view.as_deref().map(|v| v.camera.eye);
    for (entity, mut seq, parked) in &mut sequences {
        // A parked sequence's effects stop and play out, and its clock holds until its layer goes
        // live (spec 37 § 13, ADR-7200): it fires nothing while parked.
        if parked {
            if !seq.instances.is_empty() {
                for (_, id) in seq.instances.drain(..) {
                    scene.stop(id);
                }
            }
            continue;
        }
        let crash = seq.crash;
        let lights_wanted = seq.fire_lights;
        for kind in advance_crash_sequence(&mut scene, &mut seq, dt, tier, listener) {
            if kind == CrashEventKind::Detonation
                && lights_wanted
                && let Some(fires) = fire_lights.as_deref_mut()
            {
                let start = fires.frame();
                fires.spawn_crash_sites(&crash, start, &|p| Some(p));
            }
            if let (CrashEventKind::BlastArrived { distance }, Some(eye)) = (kind, listener) {
                shakes.write(CameraShakeRequest {
                    position: eye,
                    strength: crash.scale.blast_shake(distance),
                    radius: 1.0,
                });
            }
            events.write(CrashEvent {
                source: entity,
                position: crash.position,
                normal: crash.normal.normalize_or(Vec3::Y),
                kind,
            });
        }
    }
}

/// Tip bend of the grass right behind the front (a fraction of the blade height) for a blast
/// wind of `wind` m/s: the drag on a blade grows as the square of the wind and the blade bends
/// against its stiffness, so the bend grows about as the wind (the square root of the drag
/// ratio) and saturates at 0.7 near 15 m/s (ADR-1497). 4.7 m/s at shot 4's 3.3 km bends the
/// dead grass by about 0.39 of its height.
#[must_use]
pub fn grass_gust_bend(wind: f32) -> f32 {
    if !wind.is_finite() || wind <= 0.0 {
        return 0.0;
    }
    0.7 * (wind / 15.0).min(1.0).sqrt()
}

/// Publishes every running blast front into [`BlastGusts`] (after [`drive_crash_sequences`],
/// on the frame clock the sequences just stepped, so the wind reaches the camera on the frame
/// its `BlastArrived` shake does). The grass follows it through
/// `regia_framework::blast_gust` (ADR-1497).
#[tracing::instrument(skip_all)]
pub fn publish_blast_gusts(
    time: Res<Time>,
    mut gusts: ResMut<BlastGusts>,
    sequences: Query<(Entity, &CrashSequence)>,
) {
    let dt = time.delta_secs();
    let mut fronts: Vec<ActiveFront> = sequences
        .iter()
        .filter_map(|(entity, seq)| seq.active_front(entity, dt))
        .collect();
    fronts.sort_by_key(|f| f.source);
    if gusts.fronts() != fronts.as_slice() {
        gusts.set_fronts(fronts);
    }
}

/// How a vegetation layer around a camera should bend to a blast front this frame
/// ([`grass_gust_for`]): the front's centre and radii, the tip bend and the wind's length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VegetationGust {
    /// Crash point, world `[x, z]`.
    pub center: [f32; 2],
    /// Front radius this frame, metres.
    pub radius: f32,
    /// Front radius the frame before, metres.
    pub prev_radius: f32,
    /// Tip displacement right behind the front, as a fraction of the plant's height
    /// ([`grass_gust_bend`]).
    pub bend: f32,
    /// Length of the outward wind behind the front, metres (positive phase × front speed).
    pub length: f32,
}

/// The vegetation gust of the front strongest at `eye`. `None` without a front, or once its
/// tail is past the camera's surroundings.
#[must_use]
pub fn grass_gust_for(fronts: &[ActiveFront], eye: Vec3) -> Option<VegetationGust> {
    let mut set = BlastGusts::default();
    set.set_fronts(fronts.to_vec());
    let front = set.strongest_near(eye)?;
    let distance = front.origin.distance(eye);
    let wind = front.scale.blast_wind(distance);
    let length = wind.positive_phase * SHOCK_FRONT_SPEED;
    // Grass draws within a few hundred metres of the camera: done once the tail is past that.
    if front.radius() > distance + 1_000.0 + 4.0 * length {
        return None;
    }
    let bend = grass_gust_bend(wind.peak_wind);
    (bend > 0.0).then_some(VegetationGust {
        center: [front.origin.x, front.origin.z],
        radius: front.radius(),
        prev_radius: front.prev_radius(),
        bend,
        length,
    })
}

/// The acceptance numbers of a crash effect, measured with the CPU probe ([`crate::probe`]):
/// the effect fired at the origin on flat ground (`forward` = +Y), seed 7, 60 Hz. Times scale
/// with the effect's [`CrashScale`] (read from its name; a 250 t effect is measured at the
/// showcase's 15 s and 60 s). `None` for a number that does not apply to the effect.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CrashMeasure {
    /// When the flash sprite is last alive, seconds (X1: 0.05 to 0.2).
    pub flash_end: Option<f32>,
    /// Half the largest equivalent diameter of the seen flame ([`fireball_footprint`]), metres
    /// (X1: 75 to 125 at 250 t). ADR-1498: the projected flame area, not the cards' extent.
    pub fireball_peak_radius: Option<f32>,
    /// When the seen flame is largest, seconds (X1: 1 to 1.5 at 250 t).
    pub fireball_peak_time: Option<f32>,
    /// When no fire layer (flash, body, billows) is visible any more, seconds (X1: sooty by 3
    /// to 5 s at 250 t).
    pub fire_gone_at: Option<f32>,
    /// Mean rise speed of the fire body's centroid from 0.5 τ to τ, m/s (X1: 20 to 40 at
    /// 250 t).
    pub fireball_rise: Option<f32>,
    /// When the soot is measured: `τ` (3.35 s at 250 t).
    pub soot_time: Option<f32>,
    /// Soot luma (linear, before lighting) at [`Self::soot_time`] (sooty black, under 0.05).
    pub soot_luma: Option<f32>,
    /// Soot mean alpha at [`Self::soot_time`].
    pub soot_alpha: Option<f32>,
    /// Condensation shell's largest horizontal reach (p90), metres (150 to 300 at 250 t).
    pub shell_radius: Option<f32>,
    /// When the last shell puff is invisible, seconds (within 1 s).
    pub shell_gone_at: Option<f32>,
    /// Blast-front speed (p90 radius slope over the ring's middle half), m/s (343 ± 10 %).
    pub front_speed: Option<f32>,
    /// Largest relative error of the front radius against `343 t` where `343 t > 150 m`.
    pub front_error: Option<f32>,
    /// Furthest the front runs, metres (2 km at 250 t).
    pub front_reach: Option<f32>,
    /// Reach (p90) of the lifted snow and dust once it settles, metres (300 to 500 at 250 t).
    pub lift_reach: Option<f32>,
    /// Whether any lifted puff ever led the front (must be false).
    pub lift_leads_front: Option<bool>,
    /// Furthest the dust wall reaches (p90 of its outermost ring), metres (300 to 500 at 250 t,
    /// ADR-1497).
    pub wall_reach: Option<f32>,
    /// Largest lag of the dust wall's edge (the outermost visible ring's p90) behind the front,
    /// as a fraction of `343 t`, while `343 t` is between 150 m and the wall's last ring.
    pub wall_lag: Option<f32>,
    /// Whether any wall puff ever led the front by more than 2 m (must be false).
    pub wall_leads_front: Option<bool>,
    /// Tallest the wall stands (highest p90 centre height plus half the largest puff of any
    /// ring), metres.
    pub wall_height: Option<f32>,
    /// When the surge is measured: `4.5 τ` (15 s at 250 t).
    pub surge_time: Option<f32>,
    /// Surge front (p90) at [`Self::surge_time`], metres (400 to 700 at 250 t).
    pub surge_front: Option<f32>,
    /// Surge height (p90 centre height plus half the largest puff) at [`Self::surge_time`],
    /// metres (50 to 100 at 250 t).
    pub surge_height: Option<f32>,
    /// Highest p90 height the clods reach, metres (200 to 400 at 250 t).
    pub ejecta_top: Option<f32>,
    /// Landing reach (p90) of the clods once down, metres (500 to 800 at 250 t).
    pub ejecta_landing: Option<f32>,
    /// Cap top (highest puff top) at 60 s, metres (800 to 1500 at 250 t).
    pub column_top_60s: Option<f32>,
    /// Cap width (twice the p90 reach plus the largest puff) at 60 s, metres (400 to 800 at
    /// 250 t).
    pub column_width_60s: Option<f32>,
    /// Cap centroid mean rise over the first 30 s, m/s (about 25 at 250 t).
    pub column_rise_30s: Option<f32>,
    /// Cap centroid drift at 60 s, metres (downwind).
    pub column_drift_60s: Option<[f32; 3]>,
    /// Cap top at 180 s, metres (it is still there).
    pub column_top_180s: Option<f32>,
    /// Contact-puff reach (p90) at 0.5 s, metres.
    pub contact_reach: Option<f32>,
    /// When the contact puff is gone, seconds.
    pub contact_gone_at: Option<f32>,
}

/// Measures `def` (a crash effect, or any effect with the same emitter names) with the CPU
/// probe. Cost: up to a few hundred thousand probe steps (tens of milliseconds of CPU; the
/// column's 180 s run dominates).
#[must_use]
pub fn measure_crash_effect(def: &VfxSystemDef) -> CrashMeasure {
    use crate::probe::{ProbeSettings, ProbeSnapshot, probe_effect};
    let scale = CrashScale::from_effect_name(&def.name).unwrap_or(CrashScale::VEHICLE);
    let tau = scale.time_scale();
    let has = |name: &str| def.emitters.iter().any(|e| e.name == name);
    let run = |times: &[f32]| probe_effect(def, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), times);
    let steps = |step: f32, until: f32| -> Vec<f32> {
        let n = (until / step).round() as u32;
        (1..=n).map(|i| i as f32 * step).collect()
    };
    let visible = |snap: &ProbeSnapshot, name: &str| snap.emitter(name).map_or(0, |e| e.visible);
    let live = |snap: &ProbeSnapshot, name: &str| snap.emitter(name).map_or(0, |e| e.live);
    let mut out = CrashMeasure::default();
    if has("flash") {
        let snaps = run(&steps(0.01, 0.4));
        out.flash_end = snaps.iter().rev().find(|s| live(s, "flash") > 0).map(|s| s.time);
    }
    if has("fireball") {
        // The seen ball (ADR-1498): its projected flame area, not the cards' extent.
        fireball::measure_fireball(def, scale, &mut out);
    }
    if has("soot")
        && let Some(soot) = run(&[tau]).first().and_then(|s| s.emitter("soot").cloned())
    {
        out.soot_time = Some(tau);
        out.soot_luma = Some(soot.mean_luma);
        out.soot_alpha = Some(soot.mean_alpha);
    }
    if has("shell") {
        let snaps = run(&steps(0.02, 1.5));
        out.shell_radius = snaps
            .iter()
            .filter_map(|s| s.emitter("shell").filter(|e| e.visible > 0).map(|e| e.front_p90))
            .reduce(f32::max);
        out.shell_gone_at = snaps
            .iter()
            .rev()
            .find(|s| visible(s, "shell") > 0)
            .map(|s| s.time + 0.02);
    }
    if has("blast_front") {
        let ring_life = scale.blast_reach() / SHOCK_FRONT_SPEED;
        let snaps = run(&steps(ring_life / 40.0, ring_life));
        let fronts: Vec<(f32, f32)> = snaps
            .iter()
            .filter_map(|s| {
                s.emitter("blast_front")
                    .filter(|e| e.visible > 0)
                    .map(|e| (s.time, e.front_p90))
            })
            .collect();
        let mid: Vec<&(f32, f32)> = fronts
            .iter()
            .filter(|(t, _)| *t >= 0.25 * ring_life && *t <= 0.75 * ring_life)
            .collect();
        if let (Some(a), Some(b)) = (mid.first(), mid.last())
            && b.0 > a.0
        {
            out.front_speed = Some((b.1 - a.1) / (b.0 - a.0));
        }
        out.front_error = fronts
            .iter()
            .filter(|(t, _)| SHOCK_FRONT_SPEED * t > 150.0)
            .map(|(t, r)| (r - SHOCK_FRONT_SPEED * t).abs() / (SHOCK_FRONT_SPEED * t))
            .reduce(f32::max);
        out.front_reach = fronts.iter().map(|(_, r)| *r).reduce(f32::max);
    }
    if has("lifted") {
        let snaps = run(&steps(0.05, 3.0));
        out.lift_leads_front = Some(snaps.iter().any(|s| {
            s.emitter("lifted")
                .is_some_and(|e| e.live > 0 && e.front_p90 > SHOCK_FRONT_SPEED * s.time + 5.0)
        }));
        out.lift_reach = run(&[2.5 * tau])
            .first()
            .and_then(|s| s.emitter("lifted"))
            .map(|e| e.front_p90);
    }
    let wall: Vec<(&str, f32)> = def
        .emitters
        .iter()
        .filter(|e| e.name.starts_with(WALL_EMITTER_PREFIX))
        .filter_map(|e| match &e.init.velocity.mode {
            VelocityMode::Planar { speed, .. } if e.update.drag > 0.0 => {
                Some((e.name.as_str(), speed.max / e.update.drag))
            }
            _ => None,
        })
        .collect();
    if let Some(outer) = wall.iter().map(|w| w.1).reduce(f32::max) {
        let dt = ProbeSettings::default().dt;
        let snaps = run(&steps(dt, outer / SHOCK_FRONT_SPEED + 2.5 * tau));
        let mut lag = 0.0f32;
        let mut leads = false;
        let mut height = 0.0f32;
        for snap in &snaps {
            let front = blast_front_radius(snap.time);
            let mut edge = 0.0f32;
            for (name, _) in &wall {
                let Some(e) = snap.emitter(name) else { continue };
                leads |= e.live > 0 && e.front_p90 > front + 2.0;
                if e.visible > 0 {
                    edge = edge.max(e.front_p90);
                    height = height.max(e.height_p90 + 0.5 * e.max_size);
                }
            }
            if front > 150.0 && front <= outer {
                lag = lag.max((front - edge) / front);
            }
        }
        out.wall_lag = Some(lag);
        out.wall_leads_front = Some(leads);
        out.wall_height = Some(height);
        out.wall_reach = snaps
            .iter()
            .filter_map(|s| s.emitter(wall.last().map_or("", |w| w.0)))
            .filter(|e| e.visible > 0)
            .map(|e| e.front_p90)
            .reduce(f32::max);
    }
    if has("surge") {
        let at = 4.5 * tau;
        out.surge_time = Some(at);
        if let Some(e) = run(&[at]).first().and_then(|s| s.emitter("surge").cloned()) {
            out.surge_front = Some(e.front_p90);
            out.surge_height = Some(e.height_p90 + 0.5 * e.max_size);
        }
    }
    if has("clods") && has("fountain") {
        let snaps = run(&steps(0.25, 6.0 * tau));
        out.ejecta_top = snaps
            .iter()
            .filter_map(|s| s.emitter("clods").map(|e| e.height_p90))
            .reduce(f32::max);
        out.ejecta_landing = snaps.last().and_then(|s| s.emitter("clods")).map(|e| e.front_p90);
    }
    if has("cap") {
        let snaps = run(&[30.0, 60.0, 180.0]);
        if let Some(e) = snaps.first().and_then(|s| s.emitter("cap")) {
            out.column_rise_30s = Some(e.centroid.y / 30.0);
        }
        if let Some(e) = snaps.get(1).and_then(|s| s.emitter("cap")) {
            out.column_top_60s = Some(e.top);
            out.column_width_60s = Some(2.0 * e.front_p90 + e.max_size);
            out.column_drift_60s = Some(e.drift.to_array());
        }
        out.column_top_180s = snaps.get(2).and_then(|s| s.emitter("cap")).map(|e| e.top);
    }
    if has("puff") {
        let snaps = run(&steps(0.05, 2.0));
        out.contact_reach = snaps
            .iter()
            .find(|s| (s.time - 0.5).abs() < 0.01)
            .and_then(|s| s.emitter("puff"))
            .map(|e| e.front_p90);
        out.contact_gone_at = snaps
            .iter()
            .find(|s| s.time > 0.1 && visible(s, "puff") == 0)
            .map(|s| s.time);
    }
    out
}

mod fireball;

#[cfg(test)]
mod tests;
